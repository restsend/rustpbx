//! RTP payload-type knowledge: static PT table and rtpmap-name resolution.

use audio_codec::CodecType;

/// Codec + clock rate for an RTP payload type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadDescriptor {
    pub codec: CodecType,
    pub clock_rate: u32,
}

/// Resolve a payload type via the RFC 3551 static table. Dynamic PTs (and
/// unknown statics) fall back to PCMU/8000 — callers that have an SDP
/// rtpmap should prefer that mapping and only use this as the fallback.
pub fn default_payload_descriptor(pt: u8) -> PayloadDescriptor {
    match pt {
        0 => PayloadDescriptor { codec: CodecType::PCMU, clock_rate: 8000 },
        8 => PayloadDescriptor { codec: CodecType::PCMA, clock_rate: 8000 },
        9 => PayloadDescriptor { codec: CodecType::G722, clock_rate: 8000 },
        18 => PayloadDescriptor { codec: CodecType::G729, clock_rate: 8000 },
        96 | 111 => PayloadDescriptor { codec: CodecType::Opus, clock_rate: 48000 },
        _ => PayloadDescriptor { codec: CodecType::PCMU, clock_rate: 8000 },
    }
}

/// Map an SDP `a=rtpmap` encoding name to a codec.
pub fn codec_from_rtpmap_name(name: &str) -> Option<CodecType> {
    match name {
        "PCMU" => Some(CodecType::PCMU),
        "PCMA" => Some(CodecType::PCMA),
        "G722" => Some(CodecType::G722),
        "G729" => Some(CodecType::G729),
        "opus" | "OPUS" => Some(CodecType::Opus),
        "telephone-event" => Some(CodecType::TelephoneEvent),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_table() {
        assert_eq!(default_payload_descriptor(0).codec, CodecType::PCMU);
        assert_eq!(default_payload_descriptor(8).codec, CodecType::PCMA);
        assert_eq!(default_payload_descriptor(9).clock_rate, 8000);
        assert_eq!(default_payload_descriptor(18).codec, CodecType::G729);
        assert_eq!(default_payload_descriptor(111).codec, CodecType::Opus);
        assert_eq!(default_payload_descriptor(99).codec, CodecType::PCMU, "fallback");
    }

    #[test]
    fn rtpmap_names() {
        assert_eq!(codec_from_rtpmap_name("PCMU"), Some(CodecType::PCMU));
        assert_eq!(codec_from_rtpmap_name("opus"), Some(CodecType::Opus));
        assert_eq!(codec_from_rtpmap_name("VP8"), None);
    }
}
