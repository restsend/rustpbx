use audio_codec::CodecType;
use rustrtc::{Attribute, MediaKind, SdpType, SessionDescription, TransportMode};
use std::collections::{HashMap, HashSet};

/// Detect the transport mode implied by an SDP body.
///
/// A DTLS fingerprint (`a=fingerprint`) or an explicit `a=setup:` attribute
/// indicates WebRTC (DTLS/SRTP over ICE); otherwise the session is treated as
/// plain RTP. Used by [`crate::leg::Leg`] construction to pick the right
/// `RtcConfiguration.transport_mode` before the PeerConnection is created.
pub fn detect_transport(sdp: &str) -> TransportMode {
    if sdp.contains("a=fingerprint") || sdp.contains("a=setup:") {
        TransportMode::WebRtc
    } else {
        TransportMode::Rtp
    }
}

/// Parsed RTP codec information from SDP, including payload-specific parameters.
#[derive(Debug, Clone)]
pub struct CodecInfo {
    pub payload_type: u8,
    pub codec: CodecType,
    pub clock_rate: u32,
    pub channels: u16,
    /// SDP format parameters without the payload type prefix.
    pub fmtp: Option<String>,
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
            name: self.codec_name().to_string(),
            clock_rate: self.clock_rate,
            channels: Self::clamp_channels(self.channels),
        }
    }

    pub fn is_dtmf(&self) -> bool {
        self.codec == CodecType::TelephoneEvent
    }

    /// SDP/RTP codec name (e.g. `PCMU`, `opus`, `telephone-event`).
    pub fn codec_name(&self) -> &'static str {
        match self.codec {
            CodecType::PCMU => "PCMU",
            CodecType::PCMA => "PCMA",
            CodecType::G722 => "G722",
            CodecType::G729 => "G729",
            CodecType::Opus => "opus",
            CodecType::TelephoneEvent => "telephone-event",
        }
    }

    /// Convert to rustrtc AudioCapability for use in RtcConfiguration.media_capabilities
    pub fn to_audio_capability(&self) -> Option<rustrtc::config::AudioCapability> {
        use rustrtc::config::AudioCapability;
        let codec_name = self.codec_name().to_string();

        Some(AudioCapability {
            payload_type: self.payload_type,
            codec_name,
            clock_rate: self.clock_rate,
            channels: Self::clamp_channels(self.channels),
            fmtp: self.fmtp.clone(),
            rtcp_fbs: vec![],
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExtractedCodecs {
    pub audio: Vec<CodecInfo>,
    pub dtmf: Vec<CodecInfo>,
}

/// A single negotiated codec with its RTP parameters from SDP answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedCodec {
    pub codec: CodecType,
    pub payload_type: u8,
    pub clock_rate: u32,
    pub channels: u16,
}

impl NegotiatedCodec {
    /// Convert to a `CodecInfo` (no fmtp — answer codecs don't carry it).
    pub fn to_codec_info(&self) -> CodecInfo {
        CodecInfo {
            payload_type: self.payload_type,
            codec: self.codec,
            clock_rate: self.clock_rate,
            channels: self.channels,
            fmtp: None,
        }
    }
}

/// A single negotiated video codec extracted from SDP.
///
/// The codec name is kept verbatim (`H264`, etc.) — unlike the audio-only
/// [`CodecType`] used by [`NegotiatedCodec`], which cannot represent video
/// codecs. Used only for relay matching (same name → transport-level relay) and
/// rewrite-rule construction; video is never transcoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedVideoCodec {
    pub name: String,
    pub payload_type: u8,
    pub clock_rate: u32,
    pub fmtp: Option<String>,
    pub rtcp_fbs: Vec<String>,
    pub rtx_payload_type: Option<u8>,
}

impl From<rustrtc::config::VideoCapability> for NegotiatedVideoCodec {
    fn from(cap: rustrtc::config::VideoCapability) -> Self {
        Self {
            name: cap.codec_name,
            payload_type: cap.payload_type,
            clock_rate: cap.clock_rate,
            fmtp: cap.fmtp,
            rtcp_fbs: cap.rtcp_fbs,
            rtx_payload_type: cap.rtx_payload_type,
        }
    }
}

/// Per-leg negotiated media profile extracted from an SDP answer.
/// Contains the selected audio codec, the negotiated video codecs, and the selected DTMF entry for that answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedLegProfile {
    pub audio: Option<NegotiatedCodec>,
    /// All video codecs negotiated on this leg, in SDP order. Empty when the
    /// leg has no (active) video m-line.
    pub video: Vec<NegotiatedVideoCodec>,
    pub dtmf: Option<NegotiatedCodec>,
    /// ALL telephone-event payload types negotiated in the answer, not just the
    /// single preferred one (`dtmf`). A WebRTC peer may send DTMF on any of
    /// them (e.g. `126 telephone-event/8000` for browsers, even when
    /// `110 telephone-event/48000` is also negotiated for opus), so the leg's
    /// ingress tap must listen on every one of these.
    pub dtmf_pts: Vec<u8>,
    /// Transport mode for this leg (RTP or WebRTC/SRTP).
    pub transport: rustrtc::TransportMode,
}

impl Default for NegotiatedLegProfile {
    fn default() -> Self {
        Self {
            audio: None,
            video: Vec::new(),
            dtmf: None,
            dtmf_pts: Vec::new(),
            transport: rustrtc::TransportMode::Rtp,
        }
    }
}

impl NegotiatedLegProfile {
    /// All telephone-event PTs configured for this leg.
    pub fn dtmf_pts(&self) -> std::collections::HashSet<u8> {
        let mut pts: std::collections::HashSet<u8> = self.dtmf_pts.iter().copied().collect();
        if let Some(dtmf) = &self.dtmf {
            pts.insert(dtmf.payload_type);
        }
        pts
    }
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
    ) -> (HashMap<u8, CodecInfo>, HashSet<u8>, HashMap<u8, String>) {
        let mut codec_by_pt = HashMap::new();
        let mut unrecognized_pts = HashSet::new();
        let mut fmtp_by_pt = HashMap::new();

        for attr in &section.attributes {
            if attr.key == "fmtp"
                && let Some(ref value) = attr.value
                && let Some((pt_str, fmtp)) = value.trim_start().split_once(' ')
                && let Ok(pt) = pt_str.parse::<u8>()
            {
                let fmtp = fmtp.trim_start();
                if !fmtp.is_empty() {
                    fmtp_by_pt.insert(pt, fmtp.to_string());
                }
            }
        }

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
                            fmtp: fmtp_by_pt.get(&pt).cloned(),
                        },
                    );
                }
            }
        }

        (codec_by_pt, unrecognized_pts, fmtp_by_pt)
    }

    fn static_codec_for_payload(
        section: &rustrtc::MediaSection,
        pt: u8,
        fmtp: Option<String>,
    ) -> Option<CodecInfo> {
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
            fmtp,
        })
    }

    fn extract_ordered_codecs_from_section(section: &rustrtc::MediaSection) -> Vec<CodecInfo> {
        let (mut codec_by_pt, unrecognized_pts, fmtp_by_pt) =
            Self::parse_rtpmap_attributes(section);
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

            let codec = codec_by_pt.remove(&pt).or_else(|| {
                Self::static_codec_for_payload(section, pt, fmtp_by_pt.get(&pt).cloned())
            });
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

        extracted
    }

    pub fn extract_dtmf_codecs(sdp_str: &str) -> Vec<CodecInfo> {
        Self::extract_codec_params(sdp_str).dtmf
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

    pub fn codec_info_for_type(codec_type: CodecType) -> CodecInfo {
        CodecInfo {
            payload_type: codec_type.payload_type(),
            codec: codec_type,
            clock_rate: codec_type.clock_rate(),
            channels: codec_type.channels(),
            fmtp: codec_type.fmtp().map(str::to_owned),
        }
    }

    /// Build the codec capabilities for a locally generated RTP offer.
    ///
    /// Codec policy entries describe audio codecs only. Telephone-event is
    /// derived from the resulting audio clock rates so an Opus offer gets
    /// telephone-event/48000 while narrowband codecs get telephone-event/8000.
    pub fn build_local_rtp_codec_offer(codec_types: &[CodecType]) -> Vec<CodecInfo> {
        let mut result = Vec::new();
        for codec_type in codec_types.iter().copied().filter(CodecType::is_audio) {
            if !result
                .iter()
                .any(|codec: &CodecInfo| codec.codec == codec_type)
            {
                result.push(Self::codec_info_for_type(codec_type));
            }
        }

        Self::append_telephone_events_for_audio(&mut result, &[], true);
        result
    }

    fn codec_info_rtpmap(info: &CodecInfo) -> String {
        let codec_name = info.codec_name();

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
                    fmtp: CodecType::TelephoneEvent.fmtp().map(str::to_owned),
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
        let video = Self::extract_video_codecs(sdp);
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
            dtmf_pts: extracted.dtmf.iter().map(|c| c.payload_type).collect(),
            transport: rustrtc::TransportMode::Rtp,
        }
    }

    /// Extract the video codecs advertised in an SDP (offer or answer), in SDP
    /// order. Unlike [`Self::extract_codec_params`] (which drops unknown codec
    /// names through the audio-only [`CodecType`] conversion), video codec
    /// names are preserved verbatim so the supported relay codecs can be
    /// matched and unsupported codecs can be filtered explicitly.
    pub fn extract_video_codecs(sdp: &str) -> Vec<NegotiatedVideoCodec> {
        SessionDescription::parse(SdpType::Answer, sdp)
            .or_else(|_| SessionDescription::parse(SdpType::Offer, sdp))
            .map(|desc| {
                desc.media_sections
                    .into_iter()
                    .filter(|section| {
                        section.kind == rustrtc::MediaKind::Video
                            && section.port != 0
                            && section.direction != rustrtc::Direction::Inactive
                    })
                    .flat_map(|section| section.to_video_capabilities())
                    .map(NegotiatedVideoCodec::from)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Find a common pass-through video codec on both negotiated legs. Codec
    /// policy has already been applied while building each leg's SDP.
    pub fn find_common_video_codec(
        a: &[NegotiatedVideoCodec],
        b: &[NegotiatedVideoCodec],
    ) -> Option<(NegotiatedVideoCodec, NegotiatedVideoCodec)> {
        for ca in a {
            for cb in b {
                if ca.name.eq_ignore_ascii_case(&cb.name) {
                    return Some((ca.clone(), cb.clone()));
                }
            }
        }
        None
    }

    /// Build video capabilities for a leg from the remote side's offer,
    /// restricted to the configured codec allow-list while preserving the
    /// remote's payload types, fmtp, and RTCP feedback.
    ///
    /// Preserving the payload types matters: `restrict_sdp_to_reference_codecs`
    /// filters a generated answer's video PTs by (name, clock) but does NOT
    /// remap them, and RFC 3264 requires an answer to echo the offer's PTs.
    /// The fmtp is carried verbatim so the answer agrees with what the remote
    /// actually sends (relevant for H264 profile-level-id). Empty when the
    /// remote has no video or none of its codecs are in the local policy.
    pub fn video_caps_for_config(
        from: &[NegotiatedVideoCodec],
        allowed_codecs: &[String],
    ) -> Vec<rustrtc::config::VideoCapability> {
        let mut caps = Vec::new();
        for c in from {
            if !allowed_codecs
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(&c.name))
            {
                continue;
            }
            caps.push(rustrtc::config::VideoCapability {
                payload_type: c.payload_type,
                codec_name: c.name.clone(),
                clock_rate: c.clock_rate,
                fmtp: c.fmtp.clone(),
                rtcp_fbs: c.rtcp_fbs.clone(),
                rtx_payload_type: None,
            });
        }
        caps
    }

    /// Return the original offered capabilities accepted by a peer's SDP
    /// answer. Preserve the offer-side payload types because the caller uses
    /// this result to build the corresponding answer on the other leg.
    pub fn accepted_video_capabilities(
        offered: &[rustrtc::config::VideoCapability],
        peer_answer_sdp: &str,
    ) -> Vec<rustrtc::config::VideoCapability> {
        let accepted = Self::extract_video_codecs(peer_answer_sdp);
        accepted
            .iter()
            .filter_map(|accepted_cap| {
                offered
                    .iter()
                    .find(|offered_cap| {
                        offered_cap.payload_type == accepted_cap.payload_type
                            && offered_cap
                                .codec_name
                                .eq_ignore_ascii_case(&accepted_cap.name)
                            && offered_cap.clock_rate == accepted_cap.clock_rate
                    })
                    .or_else(|| {
                        offered.iter().find(|offered_cap| {
                            offered_cap
                                .codec_name
                                .eq_ignore_ascii_case(&accepted_cap.name)
                                && offered_cap.clock_rate == accepted_cap.clock_rate
                                && offered_cap.fmtp == accepted_cap.fmtp
                        })
                    })
                    .or_else(|| {
                        offered.iter().find(|offered_cap| {
                            offered_cap
                                .codec_name
                                .eq_ignore_ascii_case(&accepted_cap.name)
                                && offered_cap.clock_rate == accepted_cap.clock_rate
                        })
                    })
                    .cloned()
            })
            .collect()
    }

    /// Assign video payload types that do not collide with other media on a
    /// BUNDLE transport. Plain RTP may reuse a dynamic PT on separate audio
    /// and video sockets, but a bundled WebRTC receiver must be able to infer
    /// the media kind from the PT before the bridge rewrites the packet.
    pub fn remap_bundle_video_payload_types(
        caps: &mut [rustrtc::config::VideoCapability],
        occupied_payload_types: impl IntoIterator<Item = u8>,
    ) -> anyhow::Result<()> {
        let mut used_payload_types: HashSet<u8> = occupied_payload_types.into_iter().collect();
        for cap in caps {
            if used_payload_types.contains(&cap.payload_type) {
                cap.payload_type = (96..=127)
                    .find(|payload_type| !used_payload_types.contains(payload_type))
                    .ok_or_else(|| anyhow::anyhow!("no free dynamic video payload type"))?;
            }
            used_payload_types.insert(cap.payload_type);
        }
        Ok(())
    }

    /// Replace an SDP's video formats and codec attributes with `caps`.
    ///
    /// This only filters/reorders the video capabilities supplied by the
    /// caller. Cross-media payload-type collision handling belongs to the
    /// BUNDLE leg setup; plain RTP uses separate transports and may reuse the
    /// same payload type for audio and video.
    pub fn rewrite_video_capabilities(
        sdp_type: SdpType,
        sdp: &str,
        caps: &[rustrtc::config::VideoCapability],
    ) -> anyhow::Result<String> {
        let mut desc = SessionDescription::parse(sdp_type, sdp)
            .map_err(|error| anyhow::anyhow!("failed to parse SDP: {error}"))?;
        if let Some(video_section) = desc
            .media_sections
            .iter_mut()
            .find(|section| section.kind == MediaKind::Video)
        {
            let ordered_caps = caps.to_vec();

            if ordered_caps.is_empty() {
                video_section.port = 0;
                video_section.direction = rustrtc::Direction::Inactive;
                return Ok(desc.to_sdp_string());
            }

            video_section.formats = ordered_caps
                .iter()
                .map(|cap| cap.payload_type.to_string())
                .collect();
            video_section
                .attributes
                .retain(|attr| !matches!(attr.key.as_str(), "rtpmap" | "fmtp" | "rtcp-fb"));
            for cap in ordered_caps {
                video_section.attributes.push(Attribute::new(
                    "rtpmap",
                    Some(format!(
                        "{} {}/{}",
                        cap.payload_type, cap.codec_name, cap.clock_rate
                    )),
                ));
                if let Some(fmtp) = &cap.fmtp {
                    video_section.attributes.push(Attribute::new(
                        "fmtp",
                        Some(format!("{} {}", cap.payload_type, fmtp)),
                    ));
                }
                for feedback in &cap.rtcp_fbs {
                    video_section.attributes.push(Attribute::new(
                        "rtcp-fb",
                        Some(format!("{} {}", cap.payload_type, feedback)),
                    ));
                }
            }
        }
        Ok(desc.to_sdp_string())
    }

    /// Build codec list for an outgoing offer to the callee.
    ///
    /// Algorithm:
    /// 1. The policy (allow list, or the PBX default when empty) is used as a
    ///    filter, not an ordering. Caller-common codecs are offered first in the
    ///    CALLER'S offer order, preserving caller PT, so the codec the caller
    ///    actually transmits (its first-listed codec) is what the callee is most
    ///    likely to pick — avoiding needless transcoding.
    /// 2. Append policy codecs the caller did not offer, using local default PTs.
    ///    These extras allow transcoding when the callee cannot use any caller codec.
    /// 3. For DTMF: append telephone-event entries after the final audio codec
    ///    list, one per audio RTP clock rate, preserving the final audio clock-rate
    ///    order. Caller-offered telephone-event PTs are reused when their rate
    ///    matches; missing rates are generated locally.
    pub fn build_callee_codec_offer_with_allow(
        caller_sdp: &str,
        allow_codecs: &[CodecType],
    ) -> Vec<CodecInfo> {
        let extracted = Self::extract_codec_params(caller_sdp);
        let policy: Vec<_> = if allow_codecs.is_empty() {
            Self::default_rtp_codecs()
                .into_iter()
                .filter(|codec| *codec != CodecType::TelephoneEvent && codec.is_audio())
                .collect()
        } else {
            allow_codecs
                .iter()
                .copied()
                .filter(|codec| *codec != CodecType::TelephoneEvent && codec.is_audio())
                .collect()
        };

        let mut result: Vec<CodecInfo> = Vec::new();

        for codec in extracted.audio.iter() {
            if policy.contains(&codec.codec) && !result.iter().any(|r| r.codec == codec.codec) {
                result.push(codec.clone());
            }
        }

        for codec_type in policy {
            if !result.iter().any(|r| r.codec == codec_type) {
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
                if let Some(fmtp) = info.fmtp.as_deref() {
                    section.attributes.push(Attribute {
                        key: "fmtp".to_string(),
                        value: Some(format!("{} {}", pt, fmtp)),
                    });
                }
            }
        }

        Some(desc.to_sdp_string())
    }

    /// Force the video m-line to inactive (port 0) in an SDP answer/offer,
    /// per RFC 3264 §5.2/§6. Used when the media policy strips video from the
    /// media path (e.g. `video_policy = "strip"`): the proxy neither sends nor
    /// relays video, so the answer must not advertise a usable video m-line.
    pub fn strip_video_from_sdp(sdp: &str) -> Option<String> {
        let mut desc = SessionDescription::parse(SdpType::Offer, sdp)
            .or_else(|_| SessionDescription::parse(SdpType::Answer, sdp))
            .ok()?;
        let mut found_video = false;
        for section in desc.media_sections.iter_mut() {
            if section.kind == MediaKind::Video {
                section.port = 0;
                section.direction = rustrtc::Direction::Inactive;
                found_video = true;
            }
        }
        if found_video {
            Some(desc.to_sdp_string())
        } else {
            None
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

        let audio = if policy.is_empty() {
            // No policy: honor the offerer's own preference order.
            extracted.audio.clone()
        } else {
            // Policy present: treat it as a filter, not a reorder. Keep the
            // offerer's order for codecs shared with the policy so the answer
            // lists the caller's preferred codec first (RFC 3264 interop) and
            // the bridge ingress profile PT matches what the caller actually
            // transmits.
            let audio: Vec<_> = extracted
                .audio
                .iter()
                .filter(|codec| policy.contains(&codec.codec))
                .cloned()
                .collect();
            if audio.is_empty() {
                // No intersection: fall back to the offerer's order instead of
                // an empty audio list (which would drop the m-line).
                extracted.audio.clone()
            } else {
                audio
            }
        };

        let mut result = audio;
        let has_dtmf = !extracted.dtmf.is_empty();
        Self::append_telephone_events_for_audio(&mut result, &extracted.dtmf, false);
        if !has_dtmf {
            tracing::debug!(
                dtmf_in_offer = false,
                audio_codecs = ?result.iter().map(|c| (c.payload_type, &c.codec, c.clock_rate)).collect::<Vec<_>>(),
                "build_codec_list_from_offer: no telephone-event in caller offer, DTMF will not be added to answer"
            );
        }
        result
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn test_video_codecs() -> Vec<rustrtc::config::VideoCapability> {
        vec![rustrtc::config::VideoCapability {
            payload_type: 96,
            codec_name: "H264".to_string(),
            clock_rate: 90000,
            fmtp: Some("packetization-mode=1;profile-level-id=42e01f".to_string()),
            rtcp_fbs: vec!["nack".to_string(), "nack pli".to_string()],
            rtx_payload_type: None,
        }]
    }

    fn video_cap(
        payload_type: u8,
        codec_name: &str,
        fmtp: Option<&str>,
        rtcp_fbs: &[&str],
    ) -> rustrtc::config::VideoCapability {
        rustrtc::config::VideoCapability {
            payload_type,
            codec_name: codec_name.to_string(),
            clock_rate: 90000,
            fmtp: fmtp.map(str::to_string),
            rtcp_fbs: rtcp_fbs
                .iter()
                .map(|feedback| feedback.to_string())
                .collect(),
            rtx_payload_type: None,
        }
    }

    #[test]
    fn rewrite_video_capabilities_preserves_offer_order() {
        let generated_offer = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96 103 107 104\r\n\
a=mid:1\r\n\
a=sendrecv\r\n\
a=rtpmap:96 VP8/90000\r\n\
a=rtcp-fb:96 nack\r\n\
a=rtpmap:103 H264/90000\r\n\
a=fmtp:103 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f\r\n\
a=rtcp-fb:103 nack pli\r\n\
a=rtpmap:107 H264/90000\r\n\
a=fmtp:107 level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42001f\r\n\
a=rtcp-fb:107 nack pli\r\n\
a=rtpmap:104 VP9/90000\r\n\
a=rtcp-fb:104 nack\r\n";
        let source_caps = vec![
            video_cap(96, "H264", Some("profile-level-id=42801F"), &[]),
            video_cap(97, "VP8", None, &[]),
        ];

        let rewritten = MediaNegotiator::rewrite_video_capabilities(
            SdpType::Offer,
            generated_offer,
            &source_caps,
        )
        .unwrap();

        assert!(rewritten.contains("m=video 9 UDP/TLS/RTP/SAVPF 96 97\r\n"));
        assert!(rewritten.contains("a=rtpmap:96 H264/90000\r\n"));
        assert!(rewritten.contains("a=fmtp:96 profile-level-id=42801F\r\n"));
        assert!(rewritten.contains("a=rtpmap:97 VP8/90000\r\n"));
        assert!(!rewritten.contains("a=rtpmap:107 H264/90000\r\n"));
        assert!(!rewritten.contains("a=rtpmap:104 VP9/90000\r\n"));
        assert!(!rewritten.contains("a=rtcp-fb:104 "));
    }

    #[test]
    fn rewrite_video_capabilities_preserves_answer_payload_and_fmtp() {
        let generated_answer = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=video 4000 UDP/TLS/RTP/SAVPF 96\r\n\
a=sendrecv\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 packetization-mode=1;profile-level-id=42e01f\r\n";
        let source_caps = vec![video_cap(
            97,
            "H264",
            Some("profile-level-id=42801F"),
            &["nack pli", "ccm fir"],
        )];

        let answer = MediaNegotiator::rewrite_video_capabilities(
            SdpType::Answer,
            generated_answer,
            &source_caps,
        )
        .unwrap();

        assert!(answer.contains("m=video 4000 UDP/TLS/RTP/SAVPF 97\r\n"));
        assert!(answer.contains("a=rtpmap:97 H264/90000\r\n"));
        assert!(answer.contains("a=fmtp:97 profile-level-id=42801F\r\n"));
        assert!(answer.contains("a=rtcp-fb:97 nack pli\r\n"));
        assert!(answer.contains("a=rtcp-fb:97 ccm fir\r\n"));
        assert!(!answer.contains("a=rtpmap:96 H264/90000\r\n"));
        assert!(!answer.contains("packetization-mode=1"));
    }

    #[test]
    fn accepted_video_capabilities_keep_offer_payload_types() {
        let offered = vec![
            video_cap(96, "H264", Some("profile-level-id=42801F"), &[]),
            video_cap(98, "VP8", None, &[]),
        ];
        let peer_answer = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=video 5000 RTP/AVP 110\r\n\
a=sendrecv\r\n\
a=rtpmap:110 VP8/90000\r\n";

        let accepted = MediaNegotiator::accepted_video_capabilities(&offered, peer_answer);
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].codec_name, "VP8");
        assert_eq!(accepted[0].payload_type, 98);

        let rejected_answer = peer_answer
            .replace("m=video 5000", "m=video 0")
            .replace("a=sendrecv", "a=inactive");
        assert!(
            MediaNegotiator::accepted_video_capabilities(&offered, &rejected_answer).is_empty()
        );
    }

    #[test]
    fn video_payload_collision_is_remapped_only_for_bundle() {
        let generated_offer = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 4000 RTP/AVP 96\r\n\
a=rtpmap:96 opus/48000/2\r\n\
m=video 4002 RTP/AVP 96\r\n\
a=rtpmap:96 VP8/90000\r\n";
        let source_caps = vec![
            video_cap(96, "H264", None, &[]),
            video_cap(97, "VP8", None, &[]),
        ];

        let offer = MediaNegotiator::rewrite_video_capabilities(
            SdpType::Offer,
            generated_offer,
            &source_caps,
        )
        .expect("video offer rewrite");

        assert!(offer.contains("m=audio 4000 RTP/AVP 96\r\n"));
        assert!(offer.contains("m=video 4002 RTP/AVP 96 97\r\n"));
        assert!(offer.contains("a=rtpmap:96 H264/90000\r\n"));
        assert!(offer.contains("a=rtpmap:97 VP8/90000\r\n"));

        let mut bundle_caps = source_caps;
        MediaNegotiator::remap_bundle_video_payload_types(&mut bundle_caps, [96])
            .expect("BUNDLE video payload remap");
        assert_eq!(bundle_caps[0].payload_type, 97);
        assert_eq!(bundle_caps[1].payload_type, 98);
    }

    /// When every dynamic video payload type (96..=127) is already occupied
    /// on the BUNDLE transport, the remap must fail loudly instead of
    /// silently producing a colliding payload type.
    #[test]
    fn remap_bundle_video_payload_types_errors_when_dynamic_range_exhausted() {
        let mut caps = vec![video_cap(96, "H264", None, &[])];
        let err = MediaNegotiator::remap_bundle_video_payload_types(&mut caps, 96..=127)
            .expect_err("exhausted dynamic range must fail");
        assert!(
            err.to_string()
                .contains("no free dynamic video payload type"),
            "unexpected error: {err}"
        );
        // The original PT is untouched when the remap bails out.
        assert_eq!(caps[0].payload_type, 96);
    }

    fn video_sdp() -> &'static str {
        "v=0\r\n\
            o=- 1 1 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=video 4000 RTP/AVP 96 98\r\n\
            a=rtpmap:96 H264/90000\r\n\
            a=fmtp:96 packetization-mode=1;profile-level-id=42e01f\r\n\
            a=rtpmap:98 VP8/90000\r\n"
    }

    #[test]
    fn extract_video_codecs_preserves_h264_and_vp8_names() {
        // The audio-only CodecType parser drops unknown codec names (H264/VP8);
        // the video extractor must keep them verbatim for relay matching.
        let video = MediaNegotiator::extract_video_codecs(video_sdp());
        assert_eq!(video.len(), 2, "both video codecs kept");
        assert_eq!(video[0].name, "H264");
        assert_eq!(video[0].payload_type, 96);
        assert_eq!(video[0].clock_rate, 90000);
        assert_eq!(
            video[0].fmtp.as_deref(),
            Some("packetization-mode=1;profile-level-id=42e01f")
        );
        assert_eq!(video[1].name, "VP8");
        assert_eq!(video[1].payload_type, 98);
    }

    #[test]
    fn extract_video_codecs_handles_audio_only_sdp() {
        let sdp = "v=0\r\nm=audio 4000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
        assert!(MediaNegotiator::extract_video_codecs(sdp).is_empty());
    }

    #[test]
    fn extract_video_codecs_ignores_rejected_video_section() {
        let sdp = "v=0\r\n\
m=audio 4000 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n\
m=video 0 RTP/AVP 96\r\n\
a=inactive\r\n\
a=rtpmap:96 VP8/90000\r\n";
        assert!(MediaNegotiator::extract_video_codecs(sdp).is_empty());
    }

    #[test]
    fn extract_leg_profile_populates_video_list() {
        let sdp = format!(
            "v=0\r\nm=audio 4000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n{}",
            video_sdp()
        );
        let profile = MediaNegotiator::extract_leg_profile(&sdp);
        assert_eq!(profile.video.len(), 2);
        assert_eq!(profile.video[0].name, "H264");
    }

    #[test]
    fn find_common_video_codec_matches_h264_case_insensitive() {
        let a = vec![NegotiatedVideoCodec {
            name: "H264".into(),
            payload_type: 96,
            clock_rate: 90000,
            fmtp: None,
            rtcp_fbs: vec![],
            rtx_payload_type: None,
        }];
        let b = vec![NegotiatedVideoCodec {
            name: "h264".into(),
            payload_type: 102,
            clock_rate: 90000,
            fmtp: None,
            rtcp_fbs: vec![],
            rtx_payload_type: None,
        }];
        let matched = MediaNegotiator::find_common_video_codec(&a, &b);
        assert!(matched.is_some());
        let (ca, cb) = matched.unwrap();
        assert_eq!(ca.name, "H264");
        assert_eq!(ca.payload_type, 96);
        assert_eq!(cb.name, "h264");
        assert_eq!(cb.payload_type, 102);
    }

    #[test]
    fn find_common_video_codec_none_when_disjoint() {
        let a = vec![NegotiatedVideoCodec {
            name: "H264".into(),
            payload_type: 96,
            clock_rate: 90000,
            fmtp: None,
            rtcp_fbs: vec![],
            rtx_payload_type: None,
        }];
        let b = vec![NegotiatedVideoCodec {
            name: "VP8".into(),
            payload_type: 98,
            clock_rate: 90000,
            fmtp: None,
            rtcp_fbs: vec![],
            rtx_payload_type: None,
        }];
        assert!(MediaNegotiator::find_common_video_codec(&a, &b).is_none());
    }

    #[test]
    fn find_common_video_codec_matches_vp8() {
        let a = vec![NegotiatedVideoCodec {
            name: "VP8".into(),
            payload_type: 96,
            clock_rate: 90000,
            fmtp: None,
            rtcp_fbs: vec![],
            rtx_payload_type: None,
        }];
        let b = vec![NegotiatedVideoCodec {
            name: "vp8".into(),
            payload_type: 110,
            clock_rate: 90000,
            fmtp: None,
            rtcp_fbs: vec![],
            rtx_payload_type: None,
        }];

        let matched =
            MediaNegotiator::find_common_video_codec(&a, &b).expect("VP8 must be relay-compatible");
        assert_eq!(matched.0.payload_type, 96);
        assert_eq!(matched.1.payload_type, 110);
    }

    #[test]
    fn video_caps_for_config_preserves_remote_pt_and_fmtp() {
        let from = vec![
            NegotiatedVideoCodec {
                name: "H264".into(),
                payload_type: 102,
                clock_rate: 90000,
                fmtp: Some("packetization-mode=1;profile-level-id=640c1f".into()),
                rtcp_fbs: vec!["nack pli".into()],
                rtx_payload_type: None,
            },
            NegotiatedVideoCodec {
                name: "VP8".into(),
                payload_type: 104,
                clock_rate: 90000,
                fmtp: None,
                rtcp_fbs: vec!["nack".into()],
                rtx_payload_type: None,
            },
            // Not in this test's configured allow-list → dropped.
            NegotiatedVideoCodec {
                name: "H265".into(),
                payload_type: 106,
                clock_rate: 90000,
                fmtp: None,
                rtcp_fbs: vec![],
                rtx_payload_type: None,
            },
        ];
        let allowed = vec!["H264".to_string(), "VP8".to_string()];
        let caps = MediaNegotiator::video_caps_for_config(&from, &allowed);
        assert_eq!(caps.len(), 2, "H264 and VP8 are configured");
        assert_eq!(caps[0].payload_type, 102, "remote H264 PT preserved");
        assert_eq!(
            caps[0].fmtp.as_deref(),
            Some("packetization-mode=1;profile-level-id=640c1f")
        );
        assert_eq!(caps[0].rtcp_fbs, vec!["nack pli"]);
        assert_eq!(caps[1].codec_name, "VP8");
        assert_eq!(caps[1].payload_type, 104, "remote VP8 PT preserved");

        let h265 = vec!["H265".to_string()];
        let caps = MediaNegotiator::video_caps_for_config(&from, &h265);
        assert_eq!(caps.len(), 1, "the media helper follows its allow-list");
        assert_eq!(caps[0].codec_name, "H265");
    }

    #[test]
    fn video_caps_for_config_empty_when_no_remote_video() {
        let allowed = vec!["H264".to_string(), "VP8".to_string()];
        assert!(MediaNegotiator::video_caps_for_config(&[], &allowed).is_empty());
    }

    #[test]
    fn strip_video_from_sdp_forces_video_mline_inactive() {
        let sdp = "v=0\r\n\
            o=- 1 1 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 4000 RTP/AVP 0\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            m=video 4001 RTP/AVP 96\r\n\
            a=rtpmap:96 H264/90000\r\n\
            a=sendrecv\r\n";
        let stripped = MediaNegotiator::strip_video_from_sdp(sdp).expect("parses");
        assert!(
            stripped.contains("m=video 0 "),
            "video port must be 0:\n{stripped}"
        );
        assert!(
            stripped.contains("a=inactive"),
            "video must be inactive:\n{stripped}"
        );
        // Audio untouched.
        assert!(stripped.contains("m=audio 4000"));
    }

    #[test]
    fn strip_video_from_sdp_returns_none_when_no_video() {
        let sdp = "v=0\r\nm=audio 4000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
        assert!(MediaNegotiator::strip_video_from_sdp(sdp).is_none());
    }

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
    fn test_offer_constrained_list_is_subset_in_offer_order() {
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
        // Policy is a filter; the answer keeps the caller's offer order.
        assert_eq!(audio[0].codec, CodecType::PCMU);
        assert_eq!(audio[0].payload_type, 0);
        assert_eq!(audio[1].codec, CodecType::PCMA);
        assert_eq!(audio[1].payload_type, 8);
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

    /// Regression: caller offers PCMA first (`8 0`), allow list is `[PCMU, PCMA]`.
    /// The answer must follow the CALLER's preference (PCMA first), not the
    /// policy order — otherwise the caller keeps sending PCMA while the bridge
    /// ingress profile (derived from the answer's first codec) expects PCMU and
    /// the ForwardingTrack PT filter drops every caller packet (one-way audio).
    #[test]
    fn test_offer_constrained_list_follows_caller_order_without_peer_answer() {
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
        assert_eq!(audio[0].codec, CodecType::PCMA);
        assert_eq!(audio[0].payload_type, 8);
        assert_eq!(audio[1].codec, CodecType::PCMU);
        assert_eq!(audio[1].payload_type, 0);
    }

    /// Callee-side counterpart of the PCMA-first caller regression: the
    /// generated offer to the callee must also put the caller's preferred codec
    /// (PCMA) first so the callee is most likely to pick it, keeping the caller
    /// leg and callee leg aligned (no transcoding).
    #[test]
    fn test_callee_offer_follows_caller_order_with_pcma_first() {
        let caller_sdp = "v=0\r\n\
        o=- 1 1 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        t=0 0\r\n\
        m=audio 10000 RTP/AVP 8 0 101\r\n\
        a=rtpmap:8 PCMA/8000\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n";

        let codecs = MediaNegotiator::build_callee_codec_offer_with_allow(
            caller_sdp,
            &[CodecType::PCMU, CodecType::PCMA, CodecType::TelephoneEvent],
        );

        let audio: Vec<_> = codecs.iter().filter(|codec| !codec.is_dtmf()).collect();
        assert_eq!(audio.len(), 2);
        assert_eq!(audio[0].codec, CodecType::PCMA);
        assert_eq!(audio[0].payload_type, 8);
        assert_eq!(audio[1].codec, CodecType::PCMU);
        assert_eq!(audio[1].payload_type, 0);
        assert!(codecs.iter().any(|c| c.codec == CodecType::TelephoneEvent));
    }

    /// Reverse direction: RTP caller → WebRTC callee.
    /// The caller-facing capability list follows the caller's offer order for
    /// caller-offered codecs, and the callee offer appends policy codecs the
    /// caller did not offer.
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
            CodecType::PCMA,
            "Caller side follows caller offer order for common codecs"
        );
        assert_eq!(caller_audio[1].codec, CodecType::PCMU);
        assert_eq!(caller_audio.len(), 2);
        assert!(!caller_side.iter().any(|c| c.codec == CodecType::Opus));

        let callee_audio: Vec<_> = callee_side.iter().filter(|c| !c.is_dtmf()).collect();
        assert_eq!(callee_audio[0].codec, CodecType::PCMA);
        assert_eq!(callee_audio[1].codec, CodecType::PCMU);
        assert_eq!(callee_audio[2].codec, CodecType::Opus);
        assert_eq!(callee_audio.len(), 3);
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
                fmtp: None,
            },
            CodecInfo {
                payload_type: 8,
                codec: CodecType::PCMA,
                clock_rate: 8000,
                channels: 1,
                fmtp: None,
            },
            CodecInfo {
                payload_type: 9,
                codec: CodecType::G722,
                clock_rate: 8000,
                channels: 1,
                fmtp: None,
            },
            CodecInfo {
                payload_type: 18,
                codec: CodecType::G729,
                clock_rate: 8000,
                channels: 1,
                fmtp: None,
            },
            CodecInfo {
                payload_type: 101,
                codec: CodecType::TelephoneEvent,
                clock_rate: 8000,
                channels: 1,
                fmtp: Some("0-16".to_string()),
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

    #[test]
    fn test_codec_info_to_audio_capability_preserves_fmtp() {
        let with_fmtp = CodecInfo {
            payload_type: 96,
            codec: CodecType::Opus,
            clock_rate: 48000,
            channels: 2,
            fmtp: Some("useinbandfec=1".to_string()),
        };
        let without_fmtp = CodecInfo {
            fmtp: None,
            ..with_fmtp.clone()
        };

        assert_eq!(
            with_fmtp.to_audio_capability().unwrap().fmtp.as_deref(),
            Some("useinbandfec=1")
        );
        assert_eq!(without_fmtp.to_audio_capability().unwrap().fmtp, None);
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

    /// The allow list is a filter, not an order: when the caller offers
    /// G722(9), PCMU(0), PCMA(8) and allow_codecs = `[pcma, pcmu]`, the
    /// outgoing offer must keep the caller's order for surviving codecs
    /// (PCMU then PCMA), so the caller leg stays consistent with what the
    /// caller actually transmits.
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
            CodecType::PCMU,
            "PCMU must be first (caller offer order)"
        );
        assert_eq!(callee_audio[0].payload_type, 0, "PCMU PT must be 0");
        assert_eq!(
            callee_audio[1].codec,
            CodecType::PCMA,
            "PCMA must be second (caller offer order)"
        );
        assert_eq!(callee_audio[1].payload_type, 8, "PCMA PT must be 8");
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
        assert_eq!(dtmf[0].fmtp.as_deref(), Some("0-15"));
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
        assert_eq!(dtmf[0].fmtp.as_deref(), Some("0-15"));
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

        // PCMA (8) and PCMU (0) survive; the m-line keeps the caller's offer
        // order (PCMU before PCMA) since G722(9) was filtered out.
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
            pos_0 < pos_8,
            "PCMU (PT 0) must appear before PCMA (PT 8) in m= line (caller offer order): {}",
            m_line
        );
    }

    #[test]
    fn test_rewrite_sdp_codec_list_preserves_offered_audio_fmtp() {
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 192.0.2.10\r\n\
s=-\r\n\
c=IN IP4 192.0.2.10\r\n\
t=0 0\r\n\
m=audio 10000 RTP/AVP 96 18 101\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=fmtp:96 useinbandfec=1\r\n\
a=fmtp:18 annexb=no\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=fmtp:101 0-15\r\n";

        let selected = MediaNegotiator::build_codec_list_from_offer(
            caller_sdp,
            &[CodecType::Opus, CodecType::G729],
        );
        let rewritten = MediaNegotiator::rewrite_sdp_codec_list(caller_sdp, &selected)
            .expect("rewrite must succeed");

        assert!(rewritten.contains("a=fmtp:96 useinbandfec=1\r\n"));
        assert!(rewritten.contains("a=fmtp:18 annexb=no\r\n"));
        assert!(rewritten.contains("a=fmtp:101 0-15\r\n"));
        assert!(!rewritten.contains("stereo=1"));
        assert!(!rewritten.contains("a=fmtp:101 0-16\r\n"));
    }

    #[test]
    fn test_rewrite_sdp_codec_list_does_not_invent_fmtp_for_offered_codec() {
        let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 192.0.2.10\r\n\
s=-\r\n\
c=IN IP4 192.0.2.10\r\n\
t=0 0\r\n\
m=audio 10000 RTP/AVP 96\r\n\
a=rtpmap:96 opus/48000/2\r\n";

        let selected = MediaNegotiator::build_codec_list_from_offer(caller_sdp, &[CodecType::Opus]);
        let rewritten = MediaNegotiator::rewrite_sdp_codec_list(caller_sdp, &selected)
            .expect("rewrite must succeed");

        assert!(rewritten.contains("a=rtpmap:96 opus/48000/2\r\n"));
        assert!(!rewritten.contains("a=fmtp:96 "));
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
        assert!(
            rewritten.contains("a=fmtp:111 minptime=10;useinbandfec=1;stereo=1;sprop-stereo=1\r\n")
        );
        assert!(rewritten.contains("a=fmtp:101 0-16\r\n"));
        assert!(rewritten.contains("a=fmtp:102 0-16\r\n"));
    }

    /// A WebRTC answer can negotiate TWO telephone-event payload types — e.g.
    /// `110 telephone-event/48000` alongside `126 telephone-event/8000`.
    /// Browsers send DTMF on the 8 kHz PT (126); rustpbx must keep BOTH so the
    /// leg's ingress tap can detect DTMF regardless of which PT the peer uses.
    #[test]
    fn test_extract_leg_profile_webrtc_keeps_all_telephone_event_pts() {
        let sdp = "v=0\r\n\
            o=- 1 1 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 0.0.0.0\r\n\
            t=0 0\r\n\
            m=audio 9 UDP/TLS/RTP/SAVPF 111 9 0 8 110 126\r\n\
            a=rtpmap:111 opus/48000/2\r\n\
            a=rtpmap:9 G722/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:110 telephone-event/48000\r\n\
            a=rtpmap:126 telephone-event/8000\r\n";

        let profile = MediaNegotiator::extract_leg_profile(sdp);
        let pts = profile.dtmf_pts();
        assert!(
            pts.contains(&110),
            "must keep 48 kHz telephone-event PT 110, got {:?}",
            pts
        );
        assert!(
            pts.contains(&126),
            "must keep 8 kHz telephone-event PT 126 (browser DTMF PT), got {:?}",
            pts
        );
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
