use anyhow::{Result, anyhow};
use audio_codec::CodecType;
use rustrtc::{Attribute, MediaKind, SdpType, SessionDescription};
use std::collections::{HashMap, HashSet};

/// Parsed RTP codec information from SDP
#[derive(Debug, Clone)]
pub struct CodecInfo {
    pub payload_type: u8,
    pub codec: CodecType,
    pub clock_rate: u32,
    pub channels: u16,
}

impl CodecInfo {
    fn clamp_channels(channels: u16) -> u8 {
        if channels > u8::MAX as u16 {
            u8::MAX
        } else {
            channels as u8
        }
    }

    pub fn to_params(&self) -> rustrtc::RtpCodecParameters {
        rustrtc::RtpCodecParameters {
            payload_type: self.payload_type,
            clock_rate: self.clock_rate,
            channels: Self::clamp_channels(self.channels),
        }
    }

    pub fn is_dtmf(&self) -> bool {
        self.codec == CodecType::TelephoneEvent
    }

    /// Convert to rustrtc AudioCapability for use in RtcConfiguration.media_capabilities
    pub fn to_audio_capability(&self) -> Option<rustrtc::config::AudioCapability> {
        use rustrtc::config::AudioCapability;
        let (codec_name, default_fmtp) = match self.codec {
            CodecType::PCMU => ("PCMU".to_string(), None),
            CodecType::PCMA => ("PCMA".to_string(), None),
            CodecType::G722 => ("G722".to_string(), None),
            CodecType::G729 => ("G729".to_string(), None),
            CodecType::Opus => (
                "opus".to_string(),
                Some("minptime=10;useinbandfec=1".to_string()),
            ),
            CodecType::TelephoneEvent => ("telephone-event".to_string(), Some("0-16".to_string())),
            #[allow(unreachable_patterns)]
            _ => return None,
        };

        Some(AudioCapability {
            payload_type: self.payload_type,
            codec_name,
            clock_rate: self.clock_rate,
            channels: Self::clamp_channels(self.channels),
            fmtp: default_fmtp,
            rtcp_fbs: vec![],
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExtractedCodecs {
    pub audio: Vec<CodecInfo>,
    pub video: Vec<CodecInfo>,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedCodec {
    pub codec: CodecType,
    pub payload_type: u8,
    pub clock_rate: u32,
    pub channels: u16,
}

/// Per-leg negotiated media profile extracted from an SDP answer.
/// Contains the selected audio codec, video codec, and the selected DTMF entry for that answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedLegProfile {
    pub audio: Option<NegotiatedCodec>,
    pub video: Option<NegotiatedCodec>,
    pub dtmf: Option<NegotiatedCodec>,
    /// Transport mode for this leg (RTP or WebRTC/SRTP).
    pub transport: rustrtc::TransportMode,
}

impl Default for NegotiatedLegProfile {
    fn default() -> Self {
        Self {
            audio: None,
            video: None,
            dtmf: None,
            transport: rustrtc::TransportMode::Rtp,
        }
    }
}

/// Bridge codec lists for caller-facing and callee-facing sides.
#[derive(Debug, Clone)]
pub struct BridgeCodecLists {
    pub caller_side: Vec<CodecInfo>,
    pub callee_side: Vec<CodecInfo>,
}

/// Media negotiator for SDP parsing and codec selection
pub struct MediaNegotiator;

impl MediaNegotiator {
    fn parse_media_section(sdp_str: &str, kind: MediaKind) -> Option<rustrtc::MediaSection> {
        SessionDescription::parse(SdpType::Answer, sdp_str)
            .or_else(|_| SessionDescription::parse(SdpType::Offer, sdp_str))
            .ok()?
            .media_sections
            .into_iter()
            .find(|m| m.kind == kind)
    }

    fn parse_rtpmap_attributes(
        section: &rustrtc::MediaSection,
    ) -> (HashMap<u8, CodecInfo>, HashSet<u8>) {
        let mut codec_by_pt = HashMap::new();
        let mut unrecognized_pts = HashSet::new();

        for attr in &section.attributes {
            if attr.key == "rtpmap"
                && let Some(ref value) = attr.value
                && let Some((pt_str, codec_str)) = value.split_once(' ')
                && let Ok(pt) = pt_str.parse::<u8>()
            {
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
                        Err(_) => {
                            unrecognized_pts.insert(pt);
                            continue;
                        }
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

        (codec_by_pt, unrecognized_pts)
    }

    fn static_codec_for_payload(section: &rustrtc::MediaSection, pt: u8) -> Option<CodecInfo> {
        let static_codec = if let Ok(codec) = CodecType::try_from(pt) {
            let (rate, chans) = match codec {
                CodecType::PCMU | CodecType::PCMA | CodecType::G722 | CodecType::G729 => (8000, 1),
                CodecType::Opus => (48000, 2),
                _ => return None,
            };
            Some((codec, rate, chans))
        } else {
            if (pt == 96 || pt == 111) && section.kind == MediaKind::Audio {
                Some((CodecType::Opus, 48000, 2))
            } else {
                None
            }
        };

        static_codec.map(|(codec, rate, chans)| CodecInfo {
            payload_type: pt,
            codec,
            clock_rate: rate,
            channels: chans,
        })
    }

    fn extract_ordered_codecs_from_section(section: &rustrtc::MediaSection) -> Vec<CodecInfo> {
        let (mut codec_by_pt, unrecognized_pts) = Self::parse_rtpmap_attributes(section);
        let mut ordered_codecs = Vec::new();
        let mut seen_pts = HashSet::new();

        for format in &section.formats {
            let Ok(pt) = format.parse::<u8>() else {
                continue;
            };
            if !seen_pts.insert(pt) {
                continue;
            }

            if unrecognized_pts.contains(&pt) {
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
        let mut extracted = ExtractedCodecs::default();

        // Extract audio codecs
        if let Some(section) = Self::parse_media_section(sdp_str, MediaKind::Audio) {
            for codec in Self::extract_ordered_codecs_from_section(&section) {
                if codec.is_dtmf() {
                    extracted.dtmf.push(codec);
                } else {
                    extracted.audio.push(codec);
                }
            }
        }

        // Extract video codecs
        if let Some(section) = Self::parse_media_section(sdp_str, MediaKind::Video) {
            for codec in Self::extract_ordered_codecs_from_section(&section) {
                extracted.video.push(codec);
            }
        }

        extracted
    }

    pub fn extract_dtmf_codecs(sdp_str: &str) -> Vec<CodecInfo> {
        Self::extract_codec_params(sdp_str).dtmf
    }

    /// Select the best common codec from the remote (answerer) codec list.
    ///
    /// Rules (in order of priority):
    /// 1. Respect the remote party's SDP order (first = most preferred).
    /// 2. Skip `telephone-event` (handled separately).
    /// 3. Filter by `allow_codecs` — if empty, all codecs are allowed.
    /// 4. If the first candidate is not allowed, continue scanning the list
    ///    to find the first codec that IS in `allow_codecs`.
    pub fn select_best_codec(
        remote_codecs: &[CodecInfo],
        allowed_codecs: &[CodecType],
    ) -> Option<CodecInfo> {
        remote_codecs
            .iter()
            .filter(|c| c.codec != CodecType::TelephoneEvent)
            .find(|c| allowed_codecs.is_empty() || allowed_codecs.contains(&c.codec))
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
            CodecType::Opus,
            CodecType::G722,
            CodecType::PCMU,
            CodecType::PCMA,
            CodecType::TelephoneEvent,
        ]
    }

    pub(crate) fn codec_info_for_type(codec_type: CodecType) -> CodecInfo {
        CodecInfo {
            payload_type: codec_type.payload_type(),
            codec: codec_type,
            clock_rate: codec_type.clock_rate(),
            channels: codec_type.channels(),
        }
    }

    fn codec_info_rtpmap(info: &CodecInfo) -> String {
        let codec_name = match info.codec {
            CodecType::PCMU => "PCMU",
            CodecType::PCMA => "PCMA",
            CodecType::G722 => "G722",
            CodecType::G729 => "G729",
            CodecType::Opus => "opus",
            CodecType::TelephoneEvent => "telephone-event",
        };

        match info.channels {
            0 | 1 => format!("{}/{}", codec_name, info.clock_rate),
            channels => format!("{}/{}/{}", codec_name, info.clock_rate, channels),
        }
    }

    fn audio_clock_rates_in_order(codecs: &[CodecInfo]) -> Vec<u32> {
        let mut rates = Vec::new();
        let mut seen = HashSet::new();

        for codec in codecs {
            if codec.is_dtmf() || !codec.codec.is_audio() {
                continue;
            }
            if seen.insert(codec.clock_rate) {
                rates.push(codec.clock_rate);
            }
        }

        rates
    }

    fn next_telephone_event_payload_type(used_pts: &HashSet<u8>) -> u8 {
        let default_pt = CodecType::TelephoneEvent.payload_type();
        if !used_pts.contains(&default_pt) {
            return default_pt;
        }

        ((default_pt + 1)..=127)
            .chain(96..default_pt)
            .find(|pt| !used_pts.contains(pt))
            .unwrap_or(default_pt)
    }

    fn append_telephone_events_for_audio(
        result: &mut Vec<CodecInfo>,
        offered_dtmf: &[CodecInfo],
        generate_missing: bool,
    ) {
        let clock_rates = Self::audio_clock_rates_in_order(result);
        if clock_rates.is_empty() {
            return;
        }

        let mut used_pts: HashSet<u8> = result.iter().map(|codec| codec.payload_type).collect();
        for clock_rate in clock_rates {
            if let Some(dtmf) = offered_dtmf.iter().find(|codec| {
                codec.clock_rate == clock_rate && !used_pts.contains(&codec.payload_type)
            }) {
                used_pts.insert(dtmf.payload_type);
                result.push(dtmf.clone());
            } else if generate_missing {
                let payload_type = Self::next_telephone_event_payload_type(&used_pts);
                used_pts.insert(payload_type);
                result.push(CodecInfo {
                    payload_type,
                    codec: CodecType::TelephoneEvent,
                    clock_rate,
                    channels: 1,
                });
            }
        }
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
        let video = extracted.video.first().map(|c| NegotiatedCodec {
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
            video,
            dtmf,
            transport: rustrtc::TransportMode::Rtp,
        }
    }

    fn attr_payload_type(attr: &rustrtc::sdp::Attribute) -> Option<u8> {
        let value = attr.value.as_ref()?;
        value.split_whitespace().next()?.parse::<u8>().ok()
    }

    /// Restrict generated SDP video to the subset of codecs that also appear in
    /// a reference SDP, and mirror reference media directions.
    ///
    /// The generated SDP stays the source of truth for WebRTC-specific SDP, ICE,
    /// DTLS, and candidates. Audio codecs are not filtered here; all generated
    /// audio SDP should be rewritten by the shared codec policy helpers.
    pub fn restrict_sdp_to_reference_codecs(
        sdp_type: SdpType,
        sdp: &str,
        reference_sdp_type: SdpType,
        reference_sdp: &str,
    ) -> Option<String> {
        let mut desc = SessionDescription::parse(sdp_type, sdp).ok()?;
        let reference_desc = SessionDescription::parse(reference_sdp_type, reference_sdp).ok()?;
        let source_video_caps = desc.to_video_capabilities();
        let reference_video_caps = reference_desc.to_video_capabilities();

        if let Some(audio_section) = desc
            .media_sections
            .iter_mut()
            .find(|section| section.kind == MediaKind::Audio)
        {
            if let Some(reference_audio) = reference_desc
                .media_sections
                .iter()
                .find(|section| section.kind == MediaKind::Audio)
            {
                audio_section.direction = reference_audio.direction;
            }
        }

        if let Some(reference_video) = reference_desc
            .media_sections
            .iter()
            .find(|section| section.kind == MediaKind::Video)
            && let Some(video_section) = desc
                .media_sections
                .iter_mut()
                .find(|section| section.kind == MediaKind::Video)
        {
            let accepted_by_reference: HashSet<(String, u32)> = reference_video_caps
                .iter()
                .map(|cap| (cap.codec_name.to_ascii_uppercase(), cap.clock_rate))
                .collect();

            if !accepted_by_reference.is_empty() {
                video_section.direction = reference_video.direction;

                let mut allowed_pts = Vec::new();
                let mut seen_pts = HashSet::new();
                for cap in &source_video_caps {
                    let signature = (cap.codec_name.to_ascii_uppercase(), cap.clock_rate);
                    if accepted_by_reference.contains(&signature)
                        && seen_pts.insert(cap.payload_type)
                    {
                        allowed_pts.push(cap.payload_type);
                    }
                }

                let allowed_video_pts: HashSet<u8> = allowed_pts.into_iter().collect();
                if allowed_video_pts.is_empty() {
                    return None;
                }

                video_section.formats.retain(|pt| {
                    pt.parse::<u8>()
                        .is_ok_and(|pt| allowed_video_pts.contains(&pt))
                });
                video_section
                    .attributes
                    .retain(|attr| match attr.key.as_str() {
                        "rtpmap" | "fmtp" => Self::attr_payload_type(attr)
                            .is_none_or(|pt| allowed_video_pts.contains(&pt)),
                        "rtcp-fb" => {
                            let pt = attr
                                .value
                                .as_deref()
                                .and_then(|value| value.split_whitespace().next());
                            pt.is_none_or(|pt| {
                                pt == "*"
                                    || pt
                                        .parse::<u8>()
                                        .is_ok_and(|pt| allowed_video_pts.contains(&pt))
                            })
                        }
                        _ => true,
                    });
            }
        }

        Some(desc.to_sdp_string())
    }

    pub fn sanitize_sdp_for_rtp_peer(
        sdp_type: SdpType,
        sdp: &str,
        context: &str,
    ) -> Result<String> {
        let mut desc = SessionDescription::parse(sdp_type, sdp)
            .map_err(|e| anyhow!("Failed to parse {} SDP: {}", context, e))?;

        desc.session
            .attributes
            .retain(Self::keep_rtp_peer_session_attribute);
        for section in &mut desc.media_sections {
            section.mid.clear();
            section
                .attributes
                .retain(Self::keep_rtp_peer_media_attribute);
        }

        Ok(desc.to_sdp_string())
    }

    fn keep_rtp_peer_session_attribute(attr: &Attribute) -> bool {
        match attr.key.as_str() {
            "group" => attr
                .value
                .as_deref()
                .map(|value| !value.trim_start().starts_with("BUNDLE"))
                .unwrap_or(true),
            "msid-semantic" | "ice-lite" | "ice-ufrag" | "ice-pwd" | "ice-options"
            | "fingerprint" | "setup" | "candidate" | "end-of-candidates" | "extmap" => false,
            _ => true,
        }
    }

    fn keep_rtp_peer_media_attribute(attr: &Attribute) -> bool {
        !matches!(
            attr.key.as_str(),
            "mid"
                | "msid"
                | "bundle-only"
                | "rtcp-mux"
                | "rtcp-mux-only"
                | "rtcp-rsize"
                | "ice-ufrag"
                | "ice-pwd"
                | "ice-options"
                | "fingerprint"
                | "setup"
                | "candidate"
                | "end-of-candidates"
                | "extmap"
                | "rtcp-fb"
                | "rid"
                | "simulcast"
                | "ssrc-group"
        )
    }

    /// Build codec list for an outgoing offer to the callee.
    ///
    /// Algorithm:
    /// 1. Use the configured policy order. When policy is empty, use the PBX default order.
    /// 2. Offer caller-common codecs first in that order, preserving caller PT.
    /// 3. Append policy codecs the caller did not offer, using local default PTs. These
    ///    extras allow transcoding when the callee cannot use any caller codec.
    /// 4. For DTMF: append telephone-event entries after the final audio codec
    ///    list, one per audio RTP clock rate, preserving the final audio clock-rate
    ///    order. Caller-offered telephone-event PTs are reused when their rate
    ///    matches; missing rates are generated locally.
    pub fn build_callee_codec_offer_with_allow(
        caller_sdp: &str,
        allow_codecs: &[CodecType],
    ) -> Vec<CodecInfo> {
        let extracted = Self::extract_codec_params(caller_sdp);
        let codec_order = if allow_codecs.is_empty() {
            Self::default_rtp_codecs()
        } else {
            allow_codecs.to_vec()
        };
        let offer_order: Vec<_> = codec_order
            .into_iter()
            .filter(|codec| *codec != CodecType::TelephoneEvent && codec.is_audio())
            .collect();

        let mut result: Vec<CodecInfo> = Vec::new();

        for codec_type in &offer_order {
            if let Some(codec) = extracted
                .audio
                .iter()
                .find(|codec| codec.codec == *codec_type)
                .cloned()
            {
                result.push(codec);
            }
        }

        for codec_type in offer_order {
            if !extracted
                .audio
                .iter()
                .any(|codec| codec.codec == codec_type)
            {
                result.push(Self::codec_info_for_type(codec_type));
            }
        }

        Self::append_telephone_events_for_audio(&mut result, &extracted.dtmf, true);

        result
    }

    /// Remove codecs that should not be advertised in generated WebRTC offers.
    ///
    /// If filtering removes every audio codec, fall back to the WebRTC default
    /// offer set so the generated SDP does not contain a DTMF-only audio m-line.
    pub fn filter_webrtc_offer_codecs(caller_sdp: &str, codecs: Vec<CodecInfo>) -> Vec<CodecInfo> {
        let mut filtered: Vec<_> = codecs
            .into_iter()
            .filter(|codec| codec.codec != CodecType::G729)
            .collect();

        let audio_clock_rates: HashSet<_> = Self::audio_clock_rates_in_order(&filtered)
            .into_iter()
            .collect();
        if audio_clock_rates.is_empty() {
            return Self::build_callee_codec_offer_with_allow(
                caller_sdp,
                &Self::default_webrtc_codecs(),
            );
        }

        filtered.retain(|codec| !codec.is_dtmf() || audio_clock_rates.contains(&codec.clock_rate));
        filtered
    }

    pub fn rewrite_sdp_codec_list(sdp: &str, new_codecs: &[CodecInfo]) -> Option<String> {
        if new_codecs.is_empty() {
            return None;
        }
        let mut desc = SessionDescription::parse(SdpType::Offer, sdp)
            .or_else(|_| SessionDescription::parse(SdpType::Answer, sdp))
            .ok()?;

        if let Some(section) = desc
            .media_sections
            .iter_mut()
            .find(|m| m.kind == MediaKind::Audio)
        {
            section.formats.clear();
            section
                .attributes
                .retain(|a| !matches!(a.key.as_str(), "rtpmap" | "fmtp" | "rtcp-fb"));

            let mut seen_pts = HashSet::new();
            for info in new_codecs {
                let pt = info.payload_type;
                if !seen_pts.insert(pt) {
                    continue;
                }
                section.formats.push(pt.to_string());
                section.attributes.push(Attribute {
                    key: "rtpmap".to_string(),
                    value: Some(format!("{} {}", pt, Self::codec_info_rtpmap(info))),
                });
                if let Some(fmtp) = info.codec.fmtp() {
                    section.attributes.push(Attribute {
                        key: "fmtp".to_string(),
                        value: Some(format!("{} {}", pt, fmtp)),
                    });
                }
            }
        }

        Some(desc.to_sdp_string())
    }

    /// Build a codec list constrained by an offer and an ordered preference list.
    ///
    /// The returned audio codecs are always a subset of the offer and keep the
    /// order of `preferred_codecs`. When `preferred_codecs` is empty, or none
    /// of the preferred codecs exists in the offer, this falls back to the
    /// offer's audio codec order.
    /// Build both caller-side and callee-side codec lists for a bridge.
    ///
    /// `caller_sdp` is the SDP from the caller.
    /// `caller_is_webrtc` / `callee_is_webrtc` control WebRTC-specific filtering
    /// (e.g., G729 removal from WebRTC sides).
    /// `allow_codecs` specifies the allowed codec set (pass-through or generated).
    pub fn build_bridge_codec_lists(
        caller_sdp: &str,
        caller_is_webrtc: bool,
        callee_is_webrtc: bool,
        allow_codecs: &[CodecType],
    ) -> BridgeCodecLists {
        let mut caller_side = Self::build_codec_list_from_offer(caller_sdp, allow_codecs);
        let mut callee_side = Self::build_callee_codec_offer_with_allow(caller_sdp, allow_codecs);

        if caller_is_webrtc {
            caller_side = Self::filter_webrtc_offer_codecs(caller_sdp, caller_side);
        }
        if callee_is_webrtc {
            callee_side = Self::filter_webrtc_offer_codecs(caller_sdp, callee_side);
        }

        BridgeCodecLists {
            caller_side,
            callee_side,
        }
    }

    pub fn build_codec_list_from_offer(
        offer_sdp: &str,
        preferred_codecs: &[CodecType],
    ) -> Vec<CodecInfo> {
        let extracted = Self::extract_codec_params(offer_sdp);
        let policy: Vec<_> = preferred_codecs
            .iter()
            .copied()
            .filter(|codec| *codec != CodecType::TelephoneEvent && codec.is_audio())
            .collect();

        let mut audio = Vec::new();

        for codec_type in &policy {
            if let Some(codec) = extracted
                .audio
                .iter()
                .find(|codec| codec.codec == *codec_type)
                .cloned()
            {
                audio.push(codec);
            }
        }

        if audio.is_empty() {
            audio = extracted.audio.clone();
        }

        let mut result = audio;
        Self::append_telephone_events_for_audio(&mut result, &extracted.dtmf, false);
        result
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

        // Only allow PCMU - should scan past G722 to find PCMU
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

    // ── Bridge codec list tests ──────────────────────────────────

    /// WebRTC caller offers Opus+PCMU, allow_codecs=[PCMU] →
    /// caller side keeps PCMU only, callee side offers PCMU only (no transcode needed)
    #[test]
    fn test_bridge_codecs_webrtc_caller_rtp_callee_pcmu_only() {
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 12345 UDP/TLS/RTP/SAVPF 111 0 101\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n";

        let policy = &[CodecType::PCMU, CodecType::TelephoneEvent];
        let caller_side = MediaNegotiator::build_codec_list_from_offer(caller_sdp, policy);
        let callee_side = MediaNegotiator::build_callee_codec_offer_with_allow(caller_sdp, policy);

        // Caller side: Opus offered but filtered out (not in allow_codecs), PCMU kept
        assert!(caller_side.iter().any(|c| c.codec == CodecType::PCMU));
        assert!(
            !caller_side.iter().any(|c| c.codec == CodecType::Opus),
            "Opus not in allow_codecs"
        );
        assert!(
            caller_side
                .iter()
                .any(|c| c.codec == CodecType::TelephoneEvent)
        );

        // Callee side: only PCMU + telephone-event
        assert!(callee_side.iter().any(|c| c.codec == CodecType::PCMU));
        assert!(!callee_side.iter().any(|c| c.codec == CodecType::Opus));
        assert!(
            callee_side
                .iter()
                .any(|c| c.codec == CodecType::TelephoneEvent)
        );
    }

    /// WebRTC caller offers Opus+PCMU, allow_codecs=[Opus,PCMU] →
    /// both sides keep Opus as first codec (no transcode)
    #[test]
    fn test_bridge_codecs_prefer_no_transcode_opus() {
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 12345 UDP/TLS/RTP/SAVPF 111 0 101\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n";

        let policy = &[CodecType::Opus, CodecType::PCMU, CodecType::TelephoneEvent];
        let caller_side = MediaNegotiator::build_codec_list_from_offer(caller_sdp, policy);
        let callee_side = MediaNegotiator::build_callee_codec_offer_with_allow(caller_sdp, policy);

        // Caller side: Opus first (caller offered it, it's in allow_codecs)
        let caller_audio: Vec<_> = caller_side.iter().filter(|c| !c.is_dtmf()).collect();
        assert_eq!(
            caller_audio[0].codec,
            CodecType::Opus,
            "Opus should be first on caller side"
        );
        assert_eq!(
            caller_audio[1].codec,
            CodecType::PCMU,
            "PCMU should be second"
        );

        // Callee side: Opus first per allow_codecs order
        let callee_audio: Vec<_> = callee_side.iter().filter(|c| !c.is_dtmf()).collect();
        assert_eq!(
            callee_audio[0].codec,
            CodecType::Opus,
            "Opus should be first on callee side"
        );
        assert_eq!(callee_audio[1].codec, CodecType::PCMU);
    }

    /// RTP caller offers G729+PCMU and policy includes both.
    /// Codec policy is independent from the SDP transport envelope.
    #[test]
    fn test_bridge_codecs_keep_policy_codecs() {
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 10000 RTP/AVP 18 0 101\r\n\
a=rtpmap:18 G729/8000\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n";

        let policy = &[CodecType::G729, CodecType::PCMU, CodecType::TelephoneEvent];
        let caller_side = MediaNegotiator::build_codec_list_from_offer(caller_sdp, policy);
        let callee_side = MediaNegotiator::build_callee_codec_offer_with_allow(caller_sdp, policy);

        // Caller side: G729 is in policy and was offered.
        assert!(
            caller_side.iter().any(|c| c.codec == CodecType::G729),
            "G729 must remain on caller side"
        );
        assert!(caller_side.iter().any(|c| c.codec == CodecType::PCMU));

        assert!(
            callee_side.iter().any(|c| c.codec == CodecType::G729),
            "G729 must remain on callee side because policy owns the codec list"
        );
        assert!(
            callee_side.iter().any(|c| c.codec == CodecType::PCMU),
            "PCMU must remain on callee side"
        );
    }

    /// allow_codecs=[] means no policy restriction: generated offers can use
    /// the PBX default order, while the caller-facing side stays offer-constrained.
    #[test]
    fn test_bridge_codecs_empty_allow_codecs_fallback() {
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 10000 RTP/AVP 0 101\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n";

        let caller_side = MediaNegotiator::build_codec_list_from_offer(caller_sdp, &[]);
        let callee_side = MediaNegotiator::build_callee_codec_offer_with_allow(caller_sdp, &[]);

        let caller_audio: Vec<_> = caller_side.iter().filter(|c| !c.is_dtmf()).collect();
        assert_eq!(caller_audio.len(), 1);
        assert_eq!(caller_audio[0].codec, CodecType::PCMU);

        let callee_audio: Vec<_> = callee_side.iter().filter(|c| !c.is_dtmf()).collect();
        assert!(
            callee_audio
                .iter()
                .any(|codec| codec.codec == CodecType::PCMU),
            "Callee side should include caller-offered PCMU"
        );
        assert!(
            callee_audio
                .iter()
                .any(|codec| codec.codec == CodecType::G722),
            "Empty policy should allow PBX-default extras on generated offers"
        );
    }

    #[test]
    fn test_passthrough_offer_never_generates_missing_policy_codecs() {
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 10000 RTP/AVP 0 101\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n";

        let policy = &[CodecType::G729, CodecType::PCMU, CodecType::TelephoneEvent];
        let generated_offer =
            MediaNegotiator::build_callee_codec_offer_with_allow(caller_sdp, policy);
        assert!(
            generated_offer
                .iter()
                .any(|codec| codec.codec == CodecType::G729),
            "Generated offers can add policy codecs for transcoding"
        );

        let passthrough_offer = MediaNegotiator::build_codec_list_from_offer(caller_sdp, policy);
        let passthrough_audio: Vec<_> = passthrough_offer
            .iter()
            .filter(|codec| !codec.is_dtmf())
            .collect();
        assert_eq!(passthrough_audio.len(), 1);
        assert_eq!(passthrough_audio[0].codec, CodecType::PCMU);
        assert!(
            !passthrough_offer
                .iter()
                .any(|codec| codec.codec == CodecType::G729),
            "Pass-through offers must not advertise codecs missing from the caller offer"
        );
        assert!(
            passthrough_offer
                .iter()
                .any(|codec| codec.codec == CodecType::TelephoneEvent)
        );
    }

    /// Caller offers PCMU at PT 0 → both sides preserve caller PT 0.
    #[test]
    fn test_bridge_codecs_preserves_caller_payload_type() {
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 10000 RTP/AVP 0 101\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n";

        let policy = &[CodecType::PCMU, CodecType::TelephoneEvent];
        let caller_side = MediaNegotiator::build_codec_list_from_offer(caller_sdp, policy);
        let callee_side = MediaNegotiator::build_callee_codec_offer_with_allow(caller_sdp, policy);

        let caller_pcmu = caller_side
            .iter()
            .find(|c| c.codec == CodecType::PCMU)
            .unwrap();
        assert_eq!(
            caller_pcmu.payload_type, 0,
            "Caller side should preserve caller PT 0"
        );

        let callee_pcmu = callee_side
            .iter()
            .find(|c| c.codec == CodecType::PCMU)
            .unwrap();
        assert_eq!(
            callee_pcmu.payload_type, 0,
            "Callee side should preserve caller PT 0"
        );
    }

    /// Caller-side DTMF payload types come from the caller SDP, but only for
    /// clock rates matching the final caller-side audio codecs.
    /// Callee-side DTMF follows the final callee-side audio clock rates.
    #[test]
    fn test_bridge_codecs_preserves_caller_dtmf_payload_types_and_rates() {
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 12345 UDP/TLS/RTP/SAVPF 111 101 110\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=rtpmap:110 telephone-event/48000\r\n";

        let policy = &[CodecType::Opus, CodecType::TelephoneEvent];
        let caller_side = MediaNegotiator::build_codec_list_from_offer(caller_sdp, policy);
        let callee_side = MediaNegotiator::build_callee_codec_offer_with_allow(caller_sdp, policy);

        let caller_dtmf: Vec<_> = caller_side
            .iter()
            .filter(|c| c.codec == CodecType::TelephoneEvent)
            .collect();
        assert_eq!(caller_dtmf.len(), 1);
        assert_eq!(caller_dtmf[0].payload_type, 110);
        assert_eq!(caller_dtmf[0].clock_rate, 48000);

        let callee_dtmf: Vec<_> = callee_side
            .iter()
            .filter(|c| c.codec == CodecType::TelephoneEvent)
            .collect();
        // Callee has Opus only (no non-Opus audio), so only TE/48000 is relevant.
        assert_eq!(callee_dtmf.len(), 1);
        assert_eq!(callee_dtmf[0].payload_type, 110);
        assert_eq!(callee_dtmf[0].clock_rate, 48000);
    }

    #[test]
    fn test_callee_offer_generates_dtmf_when_caller_omits_it() {
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 10000 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n";

        let codecs =
            MediaNegotiator::build_callee_codec_offer_with_allow(caller_sdp, &[CodecType::PCMU]);

        let dtmf: Vec<_> = codecs.iter().filter(|c| c.is_dtmf()).collect();
        assert_eq!(dtmf.len(), 1);
        assert_eq!(dtmf[0].payload_type, 101);
        assert_eq!(dtmf[0].clock_rate, 8000);
    }

    #[test]
    fn test_callee_offer_appends_dtmf_in_final_audio_clock_order() {
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 10000 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n";

        let codecs = MediaNegotiator::build_callee_codec_offer_with_allow(
            caller_sdp,
            &[CodecType::PCMU, CodecType::Opus],
        );

        let audio: Vec<_> = codecs.iter().filter(|c| !c.is_dtmf()).collect();
        assert_eq!(audio[0].codec, CodecType::PCMU);
        assert_eq!(audio[1].codec, CodecType::Opus);

        let dtmf: Vec<_> = codecs.iter().filter(|c| c.is_dtmf()).collect();
        assert_eq!(dtmf.len(), 2);
        assert_eq!(dtmf[0].payload_type, 101);
        assert_eq!(dtmf[0].clock_rate, 8000);
        assert_eq!(dtmf[1].payload_type, 102);
        assert_eq!(dtmf[1].clock_rate, 48000);
    }

    #[test]
    fn test_caller_answer_filters_dtmf_by_final_audio_clock_rate() {
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 12345 UDP/TLS/RTP/SAVPF 111 0 101 110\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=rtpmap:110 telephone-event/48000\r\n";

        let codecs = MediaNegotiator::build_codec_list_from_offer(caller_sdp, &[CodecType::PCMU]);

        let audio: Vec<_> = codecs.iter().filter(|c| !c.is_dtmf()).collect();
        assert_eq!(audio.len(), 1);
        assert_eq!(audio[0].codec, CodecType::PCMU);

        let dtmf: Vec<_> = codecs.iter().filter(|c| c.is_dtmf()).collect();
        assert_eq!(dtmf.len(), 1);
        assert_eq!(dtmf[0].payload_type, 101);
        assert_eq!(dtmf[0].clock_rate, 8000);
    }

    #[test]
    fn test_caller_answer_does_not_generate_missing_dtmf() {
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 10000 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n";

        let codecs = MediaNegotiator::build_codec_list_from_offer(caller_sdp, &[CodecType::PCMU]);

        assert!(!codecs.iter().any(|c| c.is_dtmf()));
    }

    #[test]
    fn test_caller_answer_prefers_peer_answered_codec() {
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 10000 RTP/AVP 9 0 8 101\r\n\
a=rtpmap:9 G722/8000\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n";
        let codecs = MediaNegotiator::build_codec_list_from_offer(caller_sdp, &[CodecType::PCMA]);

        let audio: Vec<_> = codecs.iter().filter(|codec| !codec.is_dtmf()).collect();
        assert_eq!(audio.len(), 1);
        assert_eq!(audio[0].codec, CodecType::PCMA);
        assert_eq!(audio[0].payload_type, 8);
    }

    #[test]
    fn test_offer_constrained_list_is_subset_in_preferred_order() {
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 10000 RTP/AVP 9 0 8 101\r\n\
a=rtpmap:9 G722/8000\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n";

        let codecs = MediaNegotiator::build_codec_list_from_offer(
            caller_sdp,
            &[CodecType::PCMA, CodecType::G729, CodecType::PCMU],
        );

        let audio: Vec<_> = codecs.iter().filter(|codec| !codec.is_dtmf()).collect();
        assert_eq!(audio.len(), 2);
        assert_eq!(audio[0].codec, CodecType::PCMA);
        assert_eq!(audio[0].payload_type, 8);
        assert_eq!(audio[1].codec, CodecType::PCMU);
        assert_eq!(audio[1].payload_type, 0);
        assert!(!codecs.iter().any(|codec| codec.codec == CodecType::G729));
    }

    #[test]
    fn test_caller_answer_falls_back_to_offered_codec_for_transcoding() {
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 10000 RTP/AVP 9 101\r\n\
a=rtpmap:9 G722/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n";
        let codecs = MediaNegotiator::build_codec_list_from_offer(
            caller_sdp,
            &[CodecType::PCMA, CodecType::PCMU],
        );

        let audio: Vec<_> = codecs.iter().filter(|codec| !codec.is_dtmf()).collect();
        assert_eq!(audio.len(), 1);
        assert_eq!(audio[0].codec, CodecType::G722);
        assert_eq!(audio[0].payload_type, 9);
    }

    #[test]
    fn test_offer_constrained_list_uses_policy_order_without_peer_answer() {
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 10000 RTP/AVP 8 0 101\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n";

        let codecs = MediaNegotiator::build_codec_list_from_offer(
            caller_sdp,
            &[CodecType::PCMU, CodecType::PCMA],
        );

        let audio: Vec<_> = codecs.iter().filter(|codec| !codec.is_dtmf()).collect();
        assert_eq!(audio.len(), 2);
        assert_eq!(audio[0].codec, CodecType::PCMU);
        assert_eq!(audio[0].payload_type, 0);
        assert_eq!(audio[1].codec, CodecType::PCMA);
        assert_eq!(audio[1].payload_type, 8);
    }

    /// Reverse direction: RTP caller → WebRTC callee.
    /// The caller-facing capability list follows policy order for caller-offered codecs,
    /// and the callee offer appends policy codecs the caller did not offer.
    #[test]
    fn test_bridge_codecs_rtp_caller_webrtc_callee() {
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 10000 RTP/AVP 8 0 101\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n";

        let policy = &[
            CodecType::Opus,
            CodecType::PCMU,
            CodecType::PCMA,
            CodecType::TelephoneEvent,
        ];
        let caller_side = MediaNegotiator::build_codec_list_from_offer(caller_sdp, policy);
        let callee_side = MediaNegotiator::build_callee_codec_offer_with_allow(caller_sdp, policy);

        let caller_audio: Vec<_> = caller_side.iter().filter(|c| !c.is_dtmf()).collect();
        assert_eq!(
            caller_audio[0].codec,
            CodecType::PCMU,
            "Caller side follows policy order for common codecs"
        );
        assert_eq!(caller_audio[1].codec, CodecType::PCMA);
        assert_eq!(caller_audio.len(), 2);
        assert!(!caller_side.iter().any(|c| c.codec == CodecType::Opus));

        let callee_audio: Vec<_> = callee_side.iter().filter(|c| !c.is_dtmf()).collect();
        assert_eq!(callee_audio[0].codec, CodecType::PCMU);
        assert_eq!(callee_audio[1].codec, CodecType::PCMA);
        assert_eq!(callee_audio[2].codec, CodecType::Opus);
        assert_eq!(callee_audio.len(), 3);
    }

    #[test]
    fn test_restrict_sdp_to_reference_codecs_preserves_caller_payload_types() {
        let answer_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 96 0 110 126\r\n\
c=IN IP4 0.0.0.0\r\n\
a=mid:0\r\n\
a=sendrecv\r\n\
a=rtcp-mux\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=fmtp:96 minptime=10;useinbandfec=1\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:110 telephone-event/48000\r\n\
a=fmtp:110 0-16\r\n\
a=rtpmap:126 telephone-event/8000\r\n\
a=fmtp:126 0-16\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96 97 103 104\r\n\
c=IN IP4 0.0.0.0\r\n\
a=mid:1\r\n\
a=sendrecv\r\n\
a=rtcp-mux\r\n\
a=rtpmap:96 VP8/90000\r\n\
a=rtcp-fb:96 nack pli\r\n\
a=rtpmap:97 rtx/90000\r\n\
a=fmtp:97 apt=96\r\n\
a=rtpmap:103 H264/90000\r\n\
a=fmtp:103 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f\r\n\
a=rtcp-fb:103 nack pli\r\n\
a=rtpmap:104 rtx/90000\r\n\
a=fmtp:104 apt=103\r\n";

        let callee_answer = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 9 RTP/AVP 111 0 101\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=fmtp:111 useinbandfec=1\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=fmtp:101 0-16\r\n\
m=video 50035 RTP/AVP 103\r\n\
c=IN IP4 127.0.0.1\r\n\
a=recvonly\r\n\
a=rtpmap:103 H264/90000\r\n\
a=fmtp:103 profile-level-id=42801F; packetization-mode=1\r\n";

        let filtered = MediaNegotiator::restrict_sdp_to_reference_codecs(
            SdpType::Answer,
            answer_sdp,
            SdpType::Answer,
            callee_answer,
        )
        .unwrap();

        assert!(filtered.contains("m=audio 9 UDP/TLS/RTP/SAVPF 96 0 110 126"));
        assert!(filtered.contains("a=rtpmap:96 opus/48000/2"));
        assert!(filtered.contains("a=rtpmap:0 PCMU/8000"));
        assert!(filtered.contains("a=rtpmap:110 telephone-event/48000"));
        assert!(filtered.contains("a=rtpmap:126 telephone-event/8000"));
        assert!(!filtered.contains("a=rtpmap:101 telephone-event/8000"));
        assert!(filtered.contains("m=video 9 UDP/TLS/RTP/SAVPF 103"));
        assert!(filtered.contains("a=recvonly"));
        assert!(filtered.contains("a=rtpmap:103 H264/90000"));
        assert!(filtered.contains(
            "a=fmtp:103 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f"
        ));
        assert!(!filtered.contains("a=rtpmap:96 VP8/90000"));
        assert!(!filtered.contains("a=rtpmap:97 rtx/90000"));
        assert!(!filtered.contains("profile-level-id=42801F"));
    }

    #[test]
    fn test_restrict_answer_preserves_rtp_video_attributes_for_rtp_caller() {
        let answer_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
c=IN IP4 127.0.0.1\r\n\
t=0 0\r\n\
m=audio 18238 RTP/AVP 96 0 8 9 101 97\r\n\
a=sendrecv\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:9 G722/8000\r\n\
a=rtpmap:101 telephone-event/48000\r\n\
a=rtpmap:97 telephone-event/8000\r\n\
m=video 16756 RTP/AVP 96\r\n\
a=sendrecv\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 profile-level-id=42801F\r\n\
a=rtcp-fb:96 nack pli\r\n";

        let callee_answer = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0 1\r\n\
m=audio 50013 UDP/TLS/RTP/SAVPF 96 0 8 9 101 97\r\n\
c=IN IP4 192.168.139.3\r\n\
a=mid:0\r\n\
a=rtcp-mux\r\n\
a=rtpmap:96 opus/48000/2\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
c=IN IP4 0.0.0.0\r\n\
a=mid:1\r\n\
a=rtcp-mux\r\n\
a=rtpmap:96 H264/90000\r\n\
a=rtcp-fb:96 ccm fir\r\n\
a=rtcp-fb:96 nack pli\r\n\
a=fmtp:96 level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42001f\r\n";

        let filtered = MediaNegotiator::restrict_sdp_to_reference_codecs(
            SdpType::Answer,
            answer_sdp,
            SdpType::Answer,
            callee_answer,
        )
        .unwrap();

        assert!(filtered.contains("m=video 16756 RTP/AVP 96"));
        assert!(filtered.contains("a=rtpmap:96 H264/90000"));
        assert!(filtered.contains("a=fmtp:96 profile-level-id=42801F"));
        assert!(!filtered.contains("profile-level-id=42001f"));
    }

    #[test]
    fn test_restrict_sdp_to_reference_codecs_can_keep_transcoded_audio() {
        let answer_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111 9 0 8 126\r\n\
c=IN IP4 0.0.0.0\r\n\
a=mid:0\r\n\
a=sendrecv\r\n\
a=rtcp-mux\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=rtpmap:9 G722/8000\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:126 telephone-event/8000\r\n\
a=fmtp:126 0-16\r\n";

        let callee_answer = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 55277 RTP/AVP 18 126\r\n\
c=IN IP4 127.0.0.1\r\n\
a=sendrecv\r\n\
a=rtpmap:18 G729/8000\r\n\
a=fmtp:18 annexb=yes\r\n\
a=rtpmap:126 telephone-event/8000\r\n";

        let filtered = MediaNegotiator::restrict_sdp_to_reference_codecs(
            SdpType::Answer,
            answer_sdp,
            SdpType::Answer,
            callee_answer,
        )
        .unwrap();

        assert!(filtered.contains("m=audio 9 UDP/TLS/RTP/SAVPF 111 9 0 8 126"));
        assert!(filtered.contains("a=rtpmap:111 opus/48000/2"));
        assert!(filtered.contains("a=rtpmap:126 telephone-event/8000"));
    }

    /// to_audio_capability converts all known codecs
    #[test]
    fn test_codec_info_to_audio_capability() {
        let codecs = vec![
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
            CodecInfo {
                payload_type: 101,
                codec: CodecType::TelephoneEvent,
                clock_rate: 8000,
                channels: 1,
            },
        ];
        for ci in &codecs {
            assert!(
                ci.to_audio_capability().is_some(),
                "{:?} should convert to AudioCapability",
                ci.codec
            );
        }
    }

    /// PSTN caller offers AMR/EVS codecs at dynamic PTs 96/111.
    /// These PTs already have rtpmap entries with unrecognized codec names.
    /// The codec parser must NOT fall back to mapping PT 96/111 to Opus,
    /// because the PTs were already assigned by the caller.
    #[test]
    fn test_bridge_codecs_ignores_unrecognized_rtpmap_entries() {
        let caller_sdp = "v=0\r\n\
o=- 1777370486 1777370486 IN IP4 58.246.19.74\r\n\
s=-\r\n\
c=IN IP4 58.246.19.74\r\n\
t=0 0\r\n\
m=audio 16844 RTP/AVP 98 96 111 106 18 8 0 100\r\n\
a=rtpmap:98 AMR-WB/16000/1\r\n\
a=rtpmap:96 AMR/8000/1\r\n\
a=rtpmap:111 EVS/16000\r\n\
a=rtpmap:106 EVS/16000\r\n\
a=rtpmap:18 G729/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:100 telephone-event/8000\r\n";

        let caller_side = MediaNegotiator::build_codec_list_from_offer(caller_sdp, &[]);

        assert!(
            !caller_side.iter().any(|c| c.payload_type == 96),
            "PT 96 must NOT appear (it was AMR, not Opus)"
        );
        assert!(
            !caller_side.iter().any(|c| c.payload_type == 111),
            "PT 111 must NOT appear (it was EVS, not Opus)"
        );
        assert!(
            !caller_side.iter().any(|c| c.payload_type == 98),
            "PT 98 must NOT appear (it was AMR-WB)"
        );
        assert!(
            !caller_side.iter().any(|c| c.payload_type == 106),
            "PT 106 must NOT appear (it was EVS)"
        );

        // Recognized codecs must appear with correct caller PTs
        let has_g729 = caller_side
            .iter()
            .any(|c| c.codec == CodecType::G729 && c.payload_type == 18);
        assert!(has_g729, "G729 at PT 18 must appear in caller_side");
        let has_pcma = caller_side
            .iter()
            .any(|c| c.codec == CodecType::PCMA && c.payload_type == 8);
        assert!(has_pcma, "PCMA at PT 8 must appear in caller_side");
        let has_pcmu = caller_side
            .iter()
            .any(|c| c.codec == CodecType::PCMU && c.payload_type == 0);
        assert!(has_pcmu, "PCMU at PT 0 must appear in caller_side");

        // Verify no Opus codecs are created from PT 96/111
        assert!(
            !caller_side.iter().any(|c| c.codec == CodecType::Opus),
            "Opus must NOT appear in caller_side (PSTN didn't offer Opus)"
        );
    }

    #[test]
    fn test_performance_strategy_keeps_only_caller_codecs() {
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 10000 RTP/AVP 0 8 101\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n";

        let callee_side = MediaNegotiator::build_callee_codec_offer_with_allow(
            caller_sdp,
            &[CodecType::PCMU, CodecType::PCMA, CodecType::TelephoneEvent],
        );

        let callee_audio: Vec<_> = callee_side.iter().filter(|c| !c.is_dtmf()).collect();
        assert_eq!(
            callee_audio.len(),
            2,
            "only caller's offered codecs (no extras added)"
        );
        assert_eq!(
            callee_audio[0].codec,
            CodecType::PCMU,
            "PCMU first by policy order"
        );
        assert_eq!(
            callee_audio[1].codec,
            CodecType::PCMA,
            "PCMA second by policy order"
        );
        // G722/G729 should NOT appear (not offered by caller)
        assert!(!callee_audio.iter().any(|c| c.codec == CodecType::G722));
        assert!(!callee_audio.iter().any(|c| c.codec == CodecType::G729));
    }

    #[test]
    fn test_quality_strategy_appends_and_orders() {
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 10000 RTP/AVP 0 8 101\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n";

        let callee_side = MediaNegotiator::build_callee_codec_offer_with_allow(
            caller_sdp,
            &[
                CodecType::PCMU,
                CodecType::PCMA,
                CodecType::G722,
                CodecType::TelephoneEvent,
            ],
        );

        let callee_audio: Vec<_> = callee_side.iter().filter(|c| !c.is_dtmf()).collect();
        // Caller offered PCMU, PCMA; G722 appended as extra from allow_codecs
        // Common codecs follow policy order; extras are appended in that same policy order.
        assert_eq!(callee_audio.len(), 3, "caller codecs + appended G722");
        assert_eq!(
            callee_audio[0].codec,
            CodecType::PCMU,
            "PCMU first by policy order"
        );
        assert_eq!(
            callee_audio[1].codec,
            CodecType::PCMA,
            "PCMA second by policy order"
        );
        assert_eq!(
            callee_audio[2].codec,
            CodecType::G722,
            "G722 third (extra from allow_codecs)"
        );
    }

    /// Regression test: when allow_codecs has PCMA before PCMU (e.g. config `codecs = ["pcma", "pcmu"]`)
    /// and the caller offers G722(9), PCMU(0), PCMA(8), the outgoing offer must follow
    /// the policy order for surviving codecs.
    #[test]
    fn test_policy_order_with_pcma_first_in_allow() {
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 10000 RTP/AVP 9 0 8\r\n\
a=rtpmap:9 G722/8000\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n";

        let callee_offer = MediaNegotiator::build_callee_codec_offer_with_allow(
            caller_sdp,
            &[CodecType::PCMA, CodecType::PCMU],
        );

        let callee_audio: Vec<_> = callee_offer.iter().filter(|c| !c.is_dtmf()).collect();
        assert_eq!(callee_audio.len(), 2, "G722 must be filtered out");
        assert_eq!(
            callee_audio[0].codec,
            CodecType::PCMA,
            "PCMA must be first by policy order"
        );
        assert_eq!(callee_audio[0].payload_type, 8, "PCMA PT must be 8");
        assert_eq!(
            callee_audio[1].codec,
            CodecType::PCMU,
            "PCMU must be second by policy order"
        );
        assert_eq!(callee_audio[1].payload_type, 0, "PCMU PT must be 0");
    }

    /// Regression: wholesale trunk configured as `codecs = ["g729"]` produces
    /// `allow_codecs = [G729]` which does NOT include TelephoneEvent.
    /// `build_callee_codec_offer_with_allow` must still include telephone-event
    /// in the offer to the trunk if the caller offered it.
    #[test]
    fn test_callee_offer_includes_dtmf_when_allow_codecs_is_audio_only() {
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 10000 RTP/AVP 0 101\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=fmtp:101 0-15\r\n";

        // allow_codecs contains only G729 — no TelephoneEvent entry.
        let codecs =
            MediaNegotiator::build_callee_codec_offer_with_allow(caller_sdp, &[CodecType::G729]);

        let dtmf: Vec<_> = codecs
            .iter()
            .filter(|c| c.codec == CodecType::TelephoneEvent)
            .collect();
        assert!(
            !dtmf.is_empty(),
            "telephone-event must be included in callee offer even when allow_codecs=[G729]"
        );
        assert_eq!(
            dtmf[0].payload_type, 101,
            "telephone-event must preserve caller's PT"
        );
    }

    /// Symmetric regression: final caller answer selection must
    /// include telephone-event in the answer back to the caller even when
    /// `allow_codecs` contains only audio codecs (e.g. the PCMU-only wholesale case).
    #[test]
    fn test_caller_answer_includes_dtmf_when_allow_codecs_is_audio_only() {
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 10000 RTP/AVP 0 9 101\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:9 G729/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=fmtp:101 0-15\r\n";

        // allow_codecs=[PCMU] — caller offered PCMU, G729, and telephone-event.
        // Audio: PCMU passes, G729 filtered out.
        // DTMF: must always pass through regardless of allow_codecs.
        let codecs = MediaNegotiator::build_codec_list_from_offer(caller_sdp, &[CodecType::PCMU]);

        let audio: Vec<_> = codecs.iter().filter(|c| !c.is_dtmf()).collect();
        let dtmf: Vec<_> = codecs
            .iter()
            .filter(|c| c.codec == CodecType::TelephoneEvent)
            .collect();

        assert_eq!(audio.len(), 1, "only PCMU survives audio filtering");
        assert_eq!(audio[0].codec, CodecType::PCMU);
        assert!(
            !dtmf.is_empty(),
            "telephone-event must be included in caller answer even when allow_codecs=[PCMU]"
        );
        assert_eq!(
            dtmf[0].payload_type, 101,
            "telephone-event must preserve caller's PT"
        );
    }

    #[test]
    fn test_rewrite_sdp_codec_list_filters_and_preserves_connection() {
        // Simulates bypass mode: caller offers G722, PCMU, PCMA; allow_codecs=[PCMA, PCMU]
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 192.0.2.10\r\n\
s=-\r\n\
c=IN IP4 192.0.2.10\r\n\
t=0 0\r\n\
m=audio 10000 RTP/AVP 9 0 8\r\n\
a=rtpmap:9 G722/8000\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n";

        let new_codecs = MediaNegotiator::build_callee_codec_offer_with_allow(
            caller_sdp,
            &[CodecType::PCMA, CodecType::PCMU],
        );

        let rewritten = MediaNegotiator::rewrite_sdp_codec_list(caller_sdp, &new_codecs)
            .expect("rewrite must succeed");

        // Connection info preserved
        assert!(
            rewritten.contains("c=IN IP4 192.0.2.10"),
            "connection address must be preserved"
        );
        assert!(
            rewritten.contains("m=audio 10000"),
            "port must be preserved"
        );

        // G722 (payload 9) must be gone
        assert!(
            !rewritten.contains("a=rtpmap:9"),
            "G722 rtpmap must be removed"
        );

        // PCMA (8) must come before PCMU (0) by policy order.
        let m_line_pos = rewritten.find("m=audio").unwrap();
        let m_line_end = rewritten[m_line_pos..].find("\r\n").unwrap() + m_line_pos;
        let m_line = &rewritten[m_line_pos..m_line_end];
        let pos_0 = m_line
            .find(" 0 ")
            .or_else(|| m_line.strip_suffix(" 0").map(|_| m_line.len() - 2));
        let pos_8 = m_line
            .find(" 8 ")
            .or_else(|| m_line.strip_suffix(" 8").map(|_| m_line.len() - 2));
        assert!(
            pos_8 < pos_0,
            "PCMA (PT 8) must appear before PCMU (PT 0) in m= line: {}",
            m_line
        );
    }

    #[test]
    fn test_rewrite_sdp_codec_list_uses_dtmf_clock_rate() {
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 192.0.2.10\r\n\
s=-\r\n\
c=IN IP4 192.0.2.10\r\n\
t=0 0\r\n\
m=audio 10000 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n";

        let new_codecs = MediaNegotiator::build_callee_codec_offer_with_allow(
            caller_sdp,
            &[CodecType::PCMU, CodecType::Opus],
        );

        let rewritten = MediaNegotiator::rewrite_sdp_codec_list(caller_sdp, &new_codecs)
            .expect("rewrite must succeed");

        assert!(rewritten.contains("a=rtpmap:101 telephone-event/8000"));
        assert!(rewritten.contains("a=rtpmap:102 telephone-event/48000"));
    }

    #[test]
    fn test_negotiate_codec_g722_selected_when_first_in_local() {
        let local_codecs = vec![CodecType::G722, CodecType::PCMU, CodecType::PCMA];
        let remote_sdp = "v=0\r\n\
            o=- 1234 1234 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 10000 RTP/AVP 0 9 8 101\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:9 G722/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n";

        let result = MediaNegotiator::negotiate_codec(&local_codecs, remote_sdp).unwrap();
        assert_eq!(
            result.codec,
            CodecType::G722,
            "G722 is first in local preference and present in remote, should be selected"
        );
        assert_eq!(result.params.payload_type, 9);
    }

    #[test]
    fn test_negotiate_codec_g729_selected() {
        let local_codecs = vec![CodecType::G729, CodecType::PCMU];
        let remote_sdp = "v=0\r\n\
            o=- 1234 1234 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 10000 RTP/AVP 0 18 101\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:18 G729/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n";

        let result = MediaNegotiator::negotiate_codec(&local_codecs, remote_sdp).unwrap();
        assert_eq!(
            result.codec,
            CodecType::G729,
            "G729 is first in local preference and present in remote"
        );
        assert_eq!(result.params.payload_type, 18);
    }

    #[test]
    fn test_select_best_codec_g722_first_in_remote() {
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
        let allowed: Vec<CodecType> = vec![];
        let best = MediaNegotiator::select_best_codec(&codecs, &allowed).unwrap();
        assert_eq!(
            best.codec,
            CodecType::G722,
            "G722 first in remote, empty allow → pick G722"
        );
    }

    #[test]
    fn test_select_best_codec_g729_allowed() {
        let codecs = vec![
            CodecInfo {
                payload_type: 0,
                codec: CodecType::PCMU,
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
        let allowed = vec![CodecType::G729];
        let best = MediaNegotiator::select_best_codec(&codecs, &allowed).unwrap();
        assert_eq!(
            best.codec,
            CodecType::G729,
            "Only G729 allowed, scan past PCMU"
        );
        assert_eq!(best.payload_type, 18);
    }

    #[test]
    fn test_extract_leg_profile_g722() {
        let sdp = "v=0\r\n\
            o=- 1 1 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 127.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 10000 RTP/AVP 9 101\r\n\
            a=rtpmap:9 G722/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n";

        let profile = MediaNegotiator::extract_leg_profile(sdp);
        assert!(profile.audio.is_some());
        let audio = profile.audio.unwrap();
        assert_eq!(audio.codec, CodecType::G722);
        assert_eq!(audio.payload_type, 9);
        assert!(profile.dtmf.is_some());
    }

    #[test]
    fn test_extract_leg_profile_g729() {
        let sdp = "v=0\r\n\
            o=- 1 1 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 127.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 10000 RTP/AVP 18 0 101\r\n\
            a=rtpmap:18 G729/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n";

        let profile = MediaNegotiator::extract_leg_profile(sdp);
        assert!(profile.audio.is_some());
        let audio = profile.audio.unwrap();
        assert_eq!(
            audio.codec,
            CodecType::G729,
            "First audio codec in answer = G729"
        );
        assert_eq!(audio.payload_type, 18);
    }

    #[test]
    fn test_build_callee_offer_g722_preserves_caller_pt() {
        let caller_sdp = "v=0\r\n\
            o=- 1 1 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 10000 RTP/AVP 9 0 101\r\n\
            a=rtpmap:9 G722/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n";

        let codecs = MediaNegotiator::build_callee_codec_offer_with_allow(
            caller_sdp,
            &[CodecType::G722, CodecType::PCMU, CodecType::TelephoneEvent],
        );
        let g722 = codecs.iter().find(|c| c.codec == CodecType::G722).unwrap();
        assert_eq!(g722.payload_type, 9, "G722 PT must preserve caller's PT 9");
    }

    #[test]
    fn test_build_callee_offer_g729_preserves_caller_pt() {
        let caller_sdp = "v=0\r\n\
            o=- 1 1 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 10000 RTP/AVP 18 0 101\r\n\
            a=rtpmap:18 G729/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n";

        let codecs = MediaNegotiator::build_callee_codec_offer_with_allow(
            caller_sdp,
            &[CodecType::G729, CodecType::PCMU, CodecType::TelephoneEvent],
        );
        let g729 = codecs.iter().find(|c| c.codec == CodecType::G729).unwrap();
        assert_eq!(
            g729.payload_type, 18,
            "G729 PT must preserve caller's PT 18"
        );
    }

    #[test]
    fn test_webrtc_offer_filter_removes_g729() {
        let caller_sdp = "v=0\r\n\
            o=- 1 1 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 10000 RTP/AVP 18 0 101\r\n\
            a=rtpmap:18 G729/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n";

        let codecs = MediaNegotiator::build_callee_codec_offer_with_allow(
            caller_sdp,
            &[CodecType::G729, CodecType::PCMU, CodecType::TelephoneEvent],
        );
        let codecs = MediaNegotiator::filter_webrtc_offer_codecs(caller_sdp, codecs);
        let audio: Vec<_> = codecs.iter().filter(|c| !c.is_dtmf()).collect();
        assert_eq!(audio.len(), 1);
        assert_eq!(audio[0].codec, CodecType::PCMU);
        assert!(!codecs.iter().any(|c| c.codec == CodecType::G729));
    }

    #[test]
    fn test_webrtc_offer_filter_falls_back_when_policy_is_g729_only() {
        let caller_sdp = "v=0\r\n\
            o=- 1 1 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 10000 RTP/AVP 18 101\r\n\
            a=rtpmap:18 G729/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n";

        let codecs = MediaNegotiator::build_callee_codec_offer_with_allow(
            caller_sdp,
            &[CodecType::G729, CodecType::TelephoneEvent],
        );
        let codecs = MediaNegotiator::filter_webrtc_offer_codecs(caller_sdp, codecs);
        let audio: Vec<_> = codecs.iter().filter(|c| !c.is_dtmf()).collect();
        assert!(!audio.is_empty(), "WebRTC offer must keep an audio codec");
        assert!(!codecs.iter().any(|c| c.codec == CodecType::G729));
    }

    #[test]
    fn test_build_callee_offer_g722_with_16k_rtpmap() {
        let caller_sdp = "v=0\r\n\
            o=- 1 1 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 10000 RTP/AVP 9 0\r\n\
            a=rtpmap:9 G722/16000\r\n\
            a=rtpmap:0 PCMU/8000\r\n";

        let codecs = MediaNegotiator::build_callee_codec_offer_with_allow(caller_sdp, &[]);
        let g722 = codecs.iter().find(|c| c.codec == CodecType::G722);
        assert!(g722.is_some(), "G722 should be in callee offer");
        let g722 = g722.unwrap();
        assert_eq!(g722.payload_type, 9);
    }

    #[test]
    fn test_build_caller_answer_filters_g729_when_not_allowed() {
        let caller_sdp = "v=0\r\n\
            o=- 1 1 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 10000 RTP/AVP 18 0 9 101\r\n\
            a=rtpmap:18 G729/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:9 G722/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n";

        let codecs = MediaNegotiator::build_codec_list_from_offer(
            caller_sdp,
            &[CodecType::PCMU, CodecType::TelephoneEvent],
        );
        let audio: Vec<_> = codecs.iter().filter(|c| !c.is_dtmf()).collect();
        assert_eq!(audio.len(), 1, "Only PCMU should survive");
        assert_eq!(audio[0].codec, CodecType::PCMU);
        assert!(!codecs.iter().any(|c| c.codec == CodecType::G729));
        assert!(!codecs.iter().any(|c| c.codec == CodecType::G722));
    }
}
