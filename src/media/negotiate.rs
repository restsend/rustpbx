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
    // From RFC 3551 6.  Payload Type Definitions
    // payload type values in the range 96-127 MAY be defined dynamically through a conference control protocol
    // From RFC 4566 5.14 Media Descriptions ("m=")
    // For dynamic payload type assignments the "a=rtpmap:" attribute (see
    // Section 6) SHOULD be used to map from an RTP payload type number
    // to a media encoding name that identifies the payload format.  The
    // "a=fmtp:"  attribute MAY be used to specify format parameters (see
    // Section 6).
    pub rtp_map: Vec<(u8, (CodecType, u32, u16))>,
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
        rtp_map: Vec::new(),
    };

    match sdp.connection_information {
        Some(ref connection_information) => {
            connection_information.address.as_ref().map(|address| {
                if peer_media.rtp_addr.is_empty() {
                    peer_media.rtp_addr = address.address.clone();
                }
                if peer_media.rtcp_addr.is_empty() {
                    peer_media.rtcp_addr = address.address.clone();
                }
            });
        }
        None => {}
    }

    for media in sdp.media_descriptions.iter() {
        for attribute in media.attributes.iter() {
            if attribute.key == "rtpmap" {
                attribute.value.as_ref().map(|v| {
                    if let Ok((payload_type, codec, clock_rate, channel_count)) =
                        codecs::parse_rtpmap(v)
                    {
                        peer_media
                            .rtp_map
                            .push((payload_type, (codec, clock_rate, channel_count)));
                    }
                });
            }
        }

        if media.media_name.media == media_type {
            media.media_name.formats.iter().for_each(|format| {
                if let Ok(digit) = format.parse::<u8>() {
                    // Dynamic payload type
                    if digit >= 96 && digit <= 127 {
                        if let Some((_, (codec, _, _))) = peer_media
                            .rtp_map
                            .iter()
                            .find(|(payload_type, _)| *payload_type == digit)
                        {
                            peer_media.codecs.push(*codec);
                        } else {
                            tracing::warn!("Unknown codec type: {}", digit);
                        }
                    } else {
                        if let Ok(codec) = CodecType::try_from(digit) {
                            peer_media.codecs.push(codec);
                        }
                    }
                }
            });
            peer_media.rtp_port = media.media_name.port.value as u16;
            peer_media.rtcp_port = peer_media.rtp_port + 1;

            // Always use media-level connection info if present (overrides session-level)
            // RFC 4566 5.7.  Connection Data ("c=")
            //    A session description MUST contain either at least one "c=" field in
            //    each media description or a single "c=" field at the session level.
            //    It MAY contain a single session-level "c=" field and additional "c="
            //    field(s) per media description, in which case the per-media values
            //    override the session-level settings for the respective media.

            match media.connection_information {
                Some(ref connection_information) => {
                    connection_information.address.as_ref().map(|address| {
                        if peer_media.rtp_addr.is_empty() {
                            peer_media.rtp_addr = address.address.clone();
                        }
                        if peer_media.rtcp_addr.is_empty() {
                            peer_media.rtcp_addr = address.address.clone();
                        }
                    });
                }
                None => {}
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
