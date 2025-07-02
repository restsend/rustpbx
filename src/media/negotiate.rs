use super::codecs::{self, CodecType};
use webrtc::sdp::SessionDescription;

#[derive(Clone)]
pub struct PeerMedia {
    pub rtp_addr: String,
    pub rtp_port: u16,
    pub rtcp_addr: String,
    pub rtcp_port: u16,
    pub rtcp_mux: bool,
    pub codecs: Vec<CodecType>,
}

pub fn strip_ipv6_candidates(sdp: &str) -> String {
    sdp.lines()
        .filter(|line| !(line.starts_with("a=candidate:") && line.matches(':').count() >= 8))
        .collect::<Vec<&str>>()
        .join("\n")
        + "\n"
}

pub fn prefer_audio_codec(sdp: &SessionDescription) -> Option<codecs::CodecType> {
    let mut codecs = select_peer_media(sdp, "audio")?.codecs;
    codecs.sort_by(|a, b| a.cmp(b));
    codecs
        .iter()
        .filter(|codec| codec.is_audio())
        .last()
        .cloned()
}

pub fn select_peer_media(sdp: &SessionDescription, media_type: &str) -> Option<PeerMedia> {
    let mut peer_media = PeerMedia {
        rtp_addr: String::new(),
        rtcp_addr: String::new(),
        rtp_port: 0,
        rtcp_port: 0,
        rtcp_mux: false,
        codecs: Vec::new(),
    };

    match sdp.connection_information {
        Some(ref connection_information) => {
            connection_information.address.as_ref().map(|address| {
                peer_media.rtp_addr = address.address.clone();
                peer_media.rtcp_addr = address.address.clone();
            });
        }
        None => {}
    }
    for media in sdp.media_descriptions.iter() {
        if media.media_name.media == media_type {
            media.media_name.formats.iter().for_each(|format| {
                match CodecType::try_from(format) {
                    Ok(codec) => peer_media.codecs.push(codec),
                    Err(_) => return,
                };
            });
            peer_media.rtp_port = media.media_name.port.value as u16;
            peer_media.rtcp_port = peer_media.rtp_port + 1;

            match media.connection_information {
                Some(ref connection_information) => {
                    connection_information.address.as_ref().map(|address| {
                        peer_media.rtp_addr = address.address.clone();
                        peer_media.rtcp_addr = address.address.clone();
                    });
                }
                None => {
                    continue;
                }
            }
            for attribute in media.attributes.iter() {
                if attribute.key == "rtcp" {
                    attribute.value.as_ref().map(|v| {
                        // Parse the RTCP port from the attribute value
                        // Format is typically "port [IN IP4 address]"
                        let parts: Vec<&str> = v.split_whitespace().collect();
                        if !parts.is_empty() {
                            if let Ok(port) = parts[0].parse::<u16>() {
                                peer_media.rtcp_port = port;
                            }
                            if parts.len() >= 4 {
                                peer_media.rtcp_addr = parts[3].to_string();
                            }
                        }
                    });
                }
                if attribute.key == "rtcp-mux" {
                    peer_media.rtcp_mux = true;
                    peer_media.rtcp_addr = peer_media.rtp_addr.clone();
                    peer_media.rtcp_port = peer_media.rtp_port;
                }
            }
        }
    }
    Some(peer_media)
}

#[cfg(test)]
mod tests {
    use crate::media::{
        codecs::CodecType,
        negotiate::{prefer_audio_codec, select_peer_media},
    };
    use std::io::Cursor;
    use webrtc::sdp::SessionDescription;

    #[test]
    fn test_parse_freeswitch_sdp() {
        let offer = r#"v=0
o=FreeSWITCH 1745447592 1745447593 IN IP4 11.22.33.123
s=FreeSWITCH
c=IN IP4 11.22.33.123
t=0 0
m=audio 26328 RTP/AVP 0 101
a=rtpmap:0 PCMU/8000
a=rtpmap:101 telephone-event/8000
a=fmtp:101 0-16
a=ptime:20"#;
        let mut reader = Cursor::new(offer.as_bytes());
        let offer_sdp = SessionDescription::unmarshal(&mut reader).expect("Failed to parse SDP");
        let peer_media = select_peer_media(&offer_sdp, "audio").unwrap();
        assert_eq!(peer_media.rtp_port, 26328);
        assert_eq!(peer_media.rtcp_port, 26329);
        assert_eq!(peer_media.rtcp_addr, "11.22.33.123");
        assert_eq!(peer_media.rtp_addr, "11.22.33.123");
        assert_eq!(
            peer_media.codecs,
            vec![CodecType::PCMU, CodecType::TelephoneEvent]
        );

        let codec = prefer_audio_codec(&offer_sdp);
        assert_eq!(codec, Some(CodecType::PCMU));
    }
}
