use super::codecs;
use webrtc::sdp::SessionDescription;

pub fn strip_ipv6_candidates(sdp: &str) -> String {
    sdp.lines()
        .filter(|line| !(line.starts_with("a=candidate:") && line.matches(':').count() >= 8))
        .collect::<Vec<&str>>()
        .join("\n")
        + "\n"
}

pub fn prefer_audio_codec(sdp: &SessionDescription) -> Option<codecs::CodecType> {
    let mut formats = Vec::new();
    for media in sdp.media_descriptions.iter() {
        if media.media_name.media == "audio" {
            formats.extend(media.media_name.formats.iter());
        }
    }
    formats.sort_by(|a, b| a.cmp(b).reverse());
    for format in formats.iter() {
        match format.as_str() {
            "9" => return Some(codecs::CodecType::G722),
            "0" => return Some(codecs::CodecType::PCMU),
            "8" => return Some(codecs::CodecType::PCMA),
            _ => {}
        }
    }
    return None;
}
