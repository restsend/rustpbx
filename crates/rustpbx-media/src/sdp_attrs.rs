//! Custom SDP attribute pass-through.
//!
//! Peers (SIP trunks, SBCs, softswitches) often attach vendor-specific `a=`
//! lines (e.g. `a=x-switch-context`, `a=yx-record`) to their SDP offers and
//! answers. This module decides which attributes count as *custom*, copies
//! them across a B2BUA rewrite (offer/answer are regenerated from scratch in
//! anchored mode, which would otherwise drop them), or strips them entirely
//! when a trunk opts out.
//!
//! # Classification
//!
//! An attribute is *custom* when its key is not part of the standard SDP
//! attribute vocabulary ([`STANDARD_SDP_ATTRIBUTES`]). The default behaviour
//! is pass-through, so a false negative here (a genuinely standard attribute
//! missing from the list) only means the attribute survives a strip — the
//! conservative direction. That is why the list only contains attributes
//! defined by an RFC or produced by this stack itself.
//!
//! Note that rustrtc's parser stores every unhandled line prefix as a
//! session-level attribute (`b=AS:64` becomes `Attribute { key: "b", ... }`),
//! and serializes it back as the invalid line `a=b:AS:64`. Those prefixes are
//! therefore explicitly excluded ([`NON_ATTRIBUTE_PREFIXES`]) — they are
//! neither copied nor re-emitted by the helpers in this module.

use rustrtc::sdp::SessionDescription;

/// Standard `a=` attribute names. Anything else is treated as custom and
/// participates in pass-through / stripping.
pub const STANDARD_SDP_ATTRIBUTES: &[&str] = &[
    // RFC 4566 (session + media), RFC 3264 direction attributes.
    "sendrecv",
    "sendonly",
    "recvonly",
    "inactive",
    "ptime",
    "maxptime",
    // RFC 5888 grouping / media streams.
    "group",
    "mid",
    "msid",
    "msid-semantic",
    // RFC 4566 / RFC 4856 / RFC 5583 codec & media descriptions.
    "rtpmap",
    "fmtp",
    "rtcp",
    "rtcp-mux",
    "rtcp-rsize",
    "rtcp-fb",
    "rtcp-xr",
    "extmap",
    "extmap-allow-mixed",
    "imageattr",
    "rid",
    "simulcast",
    "content",
    // RFC 8445 / RFC 5245 ICE, RFC 5766, DTLS (RFC 5764).
    "candidate",
    "end-of-candidates",
    "ice-options",
    "ice-pwd",
    "ice-ufrag",
    "ice-lite",
    "ice-pacing",
    "fingerprint",
    "setup",
    "crypto",
    "zrtp-hash",
    // RFC 3556 / RFC 8841 (SCTP over DTLS data channels).
    "max-message-size",
    "sctp-port",
    // RFC 4796, RFC 7273 (professional media clocking).
    "mediaclk",
    "ts-refclk",
    // Attributes produced by this stack / common WebRTC dialects.
    "ssrc",
    "ssrc-group",
    "label",
];

/// SDP line prefixes that are *not* `a=` attributes. rustrtc keeps unknown
/// prefixes as session-level attributes; copying them with an `a=` prefix
/// would emit invalid SDP lines.
const NON_ATTRIBUTE_PREFIXES: &[&str] = &["b", "u", "e", "p", "r", "z", "k", "i", "c", "m", "t"];

/// Returns `true` when `key` is not a standard SDP attribute name and not a
/// non-attribute line prefix — i.e. it is a custom attribute that participates
/// in pass-through / stripping.
pub fn is_custom_attr(key: &str) -> bool {
    if NON_ATTRIBUTE_PREFIXES.contains(&key) {
        return false;
    }
    !STANDARD_SDP_ATTRIBUTES.contains(&key)
}

/// Collects the custom attribute keys present in `sdp` (session + all media
/// sections), in first-seen order. Diagnostic helper for logging.
pub fn custom_attr_keys(sdp: &SessionDescription) -> Vec<String> {
    let mut keys = Vec::new();
    let mut push = |key: &str| {
        if is_custom_attr(key) && !keys.iter().any(|k: &String| k == key) {
            keys.push(key.to_string());
        }
    };
    for attr in &sdp.session.attributes {
        push(&attr.key);
    }
    for media in &sdp.media_sections {
        for attr in &media.attributes {
            push(&attr.key);
        }
    }
    keys
}

/// Copies custom attributes from `from` into `to`:
///
/// * session-level attributes are appended to `to`'s session section;
/// * media-section attributes are copied into the `to` section carrying the
///   same `a=mid` (falling back to positional match among same-kind sections
///   when either side has no mid); sections without a counterpart in `to`
///   (e.g. video stripped from the rebuilt answer) are dropped;
/// * attributes whose key already exists in the target section are skipped,
///   so a rebuilt SDP is never polluted with duplicates.
pub fn passthrough_custom_attrs(from: &SessionDescription, to: &mut SessionDescription) {
    let mut copied = Vec::new();

    for attr in &from.session.attributes {
        if !is_custom_attr(&attr.key) {
            continue;
        }
        if to
            .session
            .attributes
            .iter()
            .any(|existing| existing.key == attr.key)
        {
            continue;
        }
        to.session.attributes.push(attr.clone());
        copied.push(attr.key.clone());
    }

    for (index, from_media) in from.media_sections.iter().enumerate() {
        let same_kind_index = from.media_sections[..index]
            .iter()
            .filter(|section| section.kind == from_media.kind)
            .count();
        let Some(target) = find_target_section(to, from_media, same_kind_index) else {
            continue;
        };
        for attr in &from_media.attributes {
            if !is_custom_attr(&attr.key) {
                continue;
            }
            if target
                .attributes
                .iter()
                .any(|existing| existing.key == attr.key)
            {
                continue;
            }
            target.attributes.push(attr.clone());
            copied.push(attr.key.clone());
        }
    }

    if !copied.is_empty() {
        tracing::debug!(attrs = ?copied, "passthrough_custom_attrs: copied custom SDP attributes");
    }
}

/// Locates the `to` media section matching `from_media`: same non-empty `mid`
/// first, then positional fallback among sections of the same media kind (the
/// rebuilt SDP may drop `a=mid` lines for non-BUNDLE offers while keeping the
/// same section order). Returns `None` when the rebuilt SDP no longer carries
/// that media kind (e.g. video stripped from the answer) — the section's
/// custom attrs are dropped.
fn find_target_section<'a>(
    to: &'a mut SessionDescription,
    from_media: &rustrtc::sdp::MediaSection,
    same_kind_index: usize,
) -> Option<&'a mut rustrtc::sdp::MediaSection> {
    if !from_media.mid.is_empty() {
        let mid_position = to
            .media_sections
            .iter()
            .position(|section| !section.mid.is_empty() && section.mid == from_media.mid);
        if let Some(position) = mid_position {
            return to.media_sections.get_mut(position);
        }
    }
    let kind = from_media.kind;
    let kind_position = to
        .media_sections
        .iter()
        .enumerate()
        .filter(|(_, section)| section.kind == kind)
        .nth(same_kind_index)
        .map(|(position, _)| position);
    kind_position.and_then(move |position| to.media_sections.get_mut(position))
}

/// Strips every custom attribute (session + media sections) from an SDP
/// string. Returns the original text untouched when parsing fails — stripping
/// is an opt-in privacy/compatibility feature, never a reason to break a call.
pub fn strip_custom_attrs_sdp(sdp: &str, sdp_type: rustrtc::SdpType) -> String {
    let Ok(mut parsed) = SessionDescription::parse(sdp_type, sdp) else {
        return sdp.to_string();
    };

    let mut removed = Vec::new();
    parsed
        .session
        .attributes
        .retain(|attr| match is_custom_attr(&attr.key) {
            true => {
                removed.push(attr.key.clone());
                false
            }
            false => true,
        });
    for media in &mut parsed.media_sections {
        media
            .attributes
            .retain(|attr| match is_custom_attr(&attr.key) {
                true => {
                    removed.push(attr.key.clone());
                    false
                }
                false => true,
            });
    }

    if !removed.is_empty() {
        tracing::debug!(attrs = ?removed, "strip_custom_attrs_sdp: removed custom SDP attributes");
    }
    parsed.to_sdp_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFER_WITH_CUSTOM: &str = "v=0\r\n\
o=- 123 123 IN IP4 192.168.1.10\r\n\
s=Talk\r\n\
c=IN IP4 192.168.1.10\r\n\
t=0 0\r\n\
a=x-session-tag:abc123\r\n\
b=AS:64\r\n\
m=audio 40000 RTP/AVP 0 101\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=fmtp:101 0-16\r\n\
a=ptime:20\r\n\
a=x-leg-color:red\r\n\
a=mid:audio0\r\n\
m=video 40002 RTP/AVP 34\r\n\
a=rtpmap:34 H264/90000\r\n\
a=x-video-marking:iso\r\n";

    fn parse(sdp: &str, sdp_type: rustrtc::SdpType) -> SessionDescription {
        SessionDescription::parse(sdp_type, sdp).expect("parse sdp")
    }

    #[test]
    fn classifies_standard_vs_custom() {
        for standard in ["rtpmap", "fmtp", "mid", "ice-ufrag", "sendrecv"] {
            assert!(!is_custom_attr(standard), "{standard} must be standard");
        }
        for custom in ["x-anything", "yx-record", "vendor-attr"] {
            assert!(is_custom_attr(custom), "{custom} must be custom");
        }
    }

    #[test]
    fn non_attribute_prefixes_are_not_custom() {
        // rustrtc stores `b=` etc. as attributes; they must never be copied
        // with an `a=` prefix.
        for prefix in ["b", "u", "e", "p", "r", "z", "k"] {
            assert!(!is_custom_attr(prefix), "prefix {prefix} must be excluded");
        }
    }

    #[test]
    fn custom_attr_keys_lists_session_and_media_keys() {
        let parsed = parse(OFFER_WITH_CUSTOM, rustrtc::SdpType::Offer);
        let keys = custom_attr_keys(&parsed);
        assert_eq!(
            keys,
            vec!["x-session-tag", "x-leg-color", "x-video-marking"]
        );
    }

    #[test]
    fn passthrough_copies_session_and_media_levels() {
        let from = parse(OFFER_WITH_CUSTOM, rustrtc::SdpType::Offer);
        // A freshly generated anchored offer: no custom attributes at all.
        let mut to = parse(
            "v=0\r\n\
o=- 456 456 IN IP4 10.0.0.1\r\n\
s=Talk\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 50000 RTP/AVP 0 101\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=mid:audio0\r\n\
a=sendrecv\r\n\
m=video 50002 RTP/AVP 34\r\n\
a=rtpmap:34 H264/90000\r\n\
a=sendrecv\r\n",
            rustrtc::SdpType::Offer,
        );

        passthrough_custom_attrs(&from, &mut to);
        let text = to.to_sdp_string();

        assert!(
            text.contains("a=x-session-tag:abc123\r\n"),
            "session-level copied"
        );
        assert!(
            !text.contains("a=b:AS:64"),
            "non-attribute prefix never copied"
        );
        let audio_section = text.split("m=video").next().unwrap();
        assert!(
            audio_section.contains("a=x-leg-color:red"),
            "audio attr in audio section"
        );
        assert!(
            text.contains("a=x-video-marking:iso"),
            "video attr in video section"
        );
        // Attribute placement: x-video-marking must live in the video section.
        let video_section = text.split("m=video").nth(1).unwrap();
        assert!(video_section.contains("a=x-video-marking:iso"));
        assert!(!audio_section.contains("x-video-marking"));
    }

    #[test]
    fn passthrough_skips_existing_keys() {
        let from = parse(OFFER_WITH_CUSTOM, rustrtc::SdpType::Offer);
        let mut to = parse(OFFER_WITH_CUSTOM, rustrtc::SdpType::Offer);
        let before = to.to_sdp_string();
        passthrough_custom_attrs(&from, &mut to);
        assert_eq!(
            to.to_sdp_string(),
            before,
            "no duplicates for existing keys"
        );
    }

    #[test]
    fn passthrough_drops_attrs_of_removed_sections() {
        let from = parse(OFFER_WITH_CUSTOM, rustrtc::SdpType::Offer);
        // Target only carries audio; the video section is gone.
        let mut to = parse(
            "v=0\r\n\
o=- 456 456 IN IP4 10.0.0.1\r\n\
s=Talk\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 50000 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n",
            rustrtc::SdpType::Offer,
        );

        passthrough_custom_attrs(&from, &mut to);
        let text = to.to_sdp_string();

        assert!(text.contains("a=x-leg-color:red"), "audio attr kept");
        assert!(
            !text.contains("x-video-marking"),
            "video attr dropped with its section"
        );
        assert!(text.contains("a=x-session-tag:abc123"), "session attr kept");
    }

    #[test]
    fn passthrough_matches_positionally_without_mid() {
        let from = parse(
            "v=0\r\n\
o=- 123 123 IN IP4 192.168.1.10\r\n\
s=Talk\r\n\
c=IN IP4 192.168.1.10\r\n\
t=0 0\r\n\
m=audio 40000 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=x-audio-tag:a\r\n\
m=application 40004 DTLS/SCTP 5000\r\n\
a=x-data-tag:d\r\n",
            rustrtc::SdpType::Offer,
        );
        let mut to = parse(
            "v=0\r\n\
o=- 456 456 IN IP4 10.0.0.1\r\n\
s=Talk\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 50000 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n\
m=application 50004 DTLS/SCTP 5000\r\n",
            rustrtc::SdpType::Offer,
        );

        passthrough_custom_attrs(&from, &mut to);
        let text = to.to_sdp_string();
        assert!(text.contains("a=x-audio-tag:a"));
        assert!(text.contains("a=x-data-tag:d"));
        let audio_section = text.split("m=application").next().unwrap();
        assert!(audio_section.contains("x-audio-tag"));
        assert!(!audio_section.contains("x-data-tag"));
    }

    #[test]
    fn strip_removes_custom_but_keeps_standard() {
        let stripped = strip_custom_attrs_sdp(OFFER_WITH_CUSTOM, rustrtc::SdpType::Offer);
        assert!(!stripped.contains("x-session-tag"));
        assert!(!stripped.contains("x-leg-color"));
        assert!(!stripped.contains("x-video-marking"));
        assert!(stripped.contains("a=rtpmap:0 PCMU/8000"));
        assert!(stripped.contains("a=ptime:20"));
        assert!(stripped.contains("m=audio 40000"), "m-lines untouched");
    }

    #[test]
    fn strip_returns_input_on_parse_failure() {
        let broken = "this is not sdp\r\n";
        assert_eq!(
            strip_custom_attrs_sdp(broken, rustrtc::SdpType::Offer),
            broken
        );
    }
}
