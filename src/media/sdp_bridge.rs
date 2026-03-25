//! SDP Bridge for WebRTC ↔ RTP interop
//!
//! This module handles SDP transformation between WebRTC and RTP formats:
//! - WebRTC: UDP/TLS/RTP/SAVPF, DTLS fingerprint, ICE candidates, SRTP
//! - RTP: RTP/AVP, plain RTP, no encryption

use anyhow::{Result, anyhow};
use rustrtc::{MediaKind, SdpType, SessionDescription};

/// ICE credentials for WebRTC connections
#[derive(Debug, Clone)]
pub struct IceCredentials {
    pub ufrag: String,
    pub pwd: String,
}

impl IceCredentials {
    /// Generate random ICE credentials
    pub fn generate() -> Self {
        use rand::RngExt;
        let mut rng = rand::rng();
        
        // Generate random ufrag (4-8 characters as per RFC 5245)
        const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        let ufrag_len = rng.random_range(4..=8);
        let ufrag: String = (0..ufrag_len)
            .map(|_| CHARS[rng.random_range(0..CHARS.len())] as char)
            .collect();
        
        // Generate random pwd (22-256 characters as per RFC 5245)
        let pwd_len = rng.random_range(22..=32);
        let pwd: String = (0..pwd_len)
            .map(|_| CHARS[rng.random_range(0..CHARS.len())] as char)
            .collect();
        
        Self { ufrag, pwd }
    }
}

/// DTLS certificate info for WebRTC connections
#[derive(Debug, Clone)]
pub struct DtlsInfo {
    pub fingerprint: String,
    pub setup: String,
}

impl DtlsInfo {
    /// Generate a placeholder DTLS fingerprint
    /// 
    /// TODO: In production, this should be derived from the actual DTLS certificate
    /// stored in the server's TLS configuration
    pub fn generate_placeholder() -> Self {
        // Generate a random SHA-256 fingerprint
        // In production, this should come from the actual certificate
        use rand::RngExt;
        let mut rng = rand::rng();
        let bytes: Vec<u8> = (0..32).map(|_| rng.random::<u8>()).collect();
        let fingerprint = bytes
            .iter()
            .map(|b| format!("{:02X}", b))
            .collect::<Vec<_>>()
            .join(":");
        
        Self {
            fingerprint,
            setup: "actpass".to_string(),
        }
    }
    
    /// Get fingerprint from server configuration
    /// 
    /// This should be called with the server's TLS certificate fingerprint
    pub fn from_certificate(fingerprint: &str) -> Self {
        Self {
            fingerprint: fingerprint.to_string(),
            setup: "actpass".to_string(),
        }
    }
}

/// SDP format type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdpFormat {
    /// WebRTC SDP with DTLS, ICE, SRTP
    WebRtc,
    /// Plain RTP SDP
    Rtp,
}

/// Detect SDP format from content
pub fn detect_sdp_format(sdp: &str) -> SdpFormat {
    if sdp.contains("UDP/TLS/RTP/SAVPF")
        || sdp.contains("a=fingerprint:")
        || sdp.contains("a=ice-ufrag:")
    {
        SdpFormat::WebRtc
    } else {
        SdpFormat::Rtp
    }
}

/// SDP Bridge for converting between WebRTC and RTP formats
pub struct SdpBridge;

impl SdpBridge {
    /// Convert WebRTC SDP to RTP SDP
    /// 
    /// Changes:
    /// - Protocol: UDP/TLS/RTP/SAVPF → RTP/AVP
    /// - Remove: fingerprint, ice-ufrag, ice-pwd, setup, rtcp-mux
    /// - Add: standard RTP port
    /// - Codec: keep compatible codecs (Opus → optional, PCMU/PCMA preferred)
    pub fn webrtc_to_rtp(webrtc_sdp: &str) -> Result<String> {
        let parsed = SessionDescription::parse(SdpType::Offer, webrtc_sdp)
            .or_else(|_| SessionDescription::parse(SdpType::Answer, webrtc_sdp))
            .map_err(|e| anyhow!("Failed to parse WebRTC SDP: {}", e))?;

        let mut rtp_sdp = String::new();

        // Add session header
        rtp_sdp.push_str(&format!("v=0\r\n"));
        rtp_sdp.push_str(&format!(
            "o=- {} {} IN IP4 {}\r\n",
            parsed.session.origin.session_id,
            parsed.session.origin.session_version,
            parsed.session.origin.unicast_address
        ));
        rtp_sdp.push_str("s=RustPBX Bridge\r\n");
        rtp_sdp.push_str(&format!("c=IN IP4 {}\r\n", parsed.session.origin.unicast_address));
        rtp_sdp.push_str("t=0 0\r\n");

        // Transform media sections
        for section in &parsed.media_sections {
            if section.kind != MediaKind::Audio {
                continue; // Skip non-audio for now
            }

            // Convert protocol and build format list
            let mut rtp_formats = Vec::new();
            let mut rtp_attrs = Vec::new();

            for format in &section.formats {
                if let Ok(pt) = format.parse::<u8>() {
                    rtp_formats.push(pt);
                }
            }

            // Find RTP map attributes for our formats
            let mut codec_found = false;
            for attr in &section.attributes {
                if attr.key == "rtpmap" {
                    if let Some(ref value) = attr.value {
                        // Parse rtpmap: <pt> <codec>/<rate>
                        let parts: Vec<&str> = value.splitn(2, ' ').collect();
                        if parts.len() == 2 {
                            let pt_str = parts[0];
                            let codec_info = parts[1];
                            
                            // Keep PCMU, PCMA, telephone-event (skip Opus for RTP)
                            if codec_info.contains("PCMU")
                                || codec_info.contains("PCMA")
                                || codec_info.contains("telephone-event")
                                || codec_info.contains("G722")
                                || codec_info.contains("G729")
                            {
                                rtp_attrs.push(format!("a=rtpmap:{} {}\r\n", pt_str, codec_info));
                                codec_found = true;
                            }
                        }
                    }
                } else if attr.key == "fmtp" {
                    // Keep fmtp for telephone-event
                    if let Some(ref value) = attr.value {
                        if value.contains("101") || value.contains("telephone-event") {
                            rtp_attrs.push(format!("a=fmtp:{}\r\n", value));
                        }
                    }
                } else if attr.key == "sendrecv"
                    || attr.key == "sendonly"
                    || attr.key == "recvonly"
                    || attr.key == "inactive"
                {
                    rtp_attrs.push(format!("a={}\r\n", attr.key));
                }
                // Skip: fingerprint, setup, ice-ufrag, ice-pwd, rtcp-mux, candidate, mid
            }

            // If no compatible codec found, add default PCMU
            if !codec_found {
                rtp_formats = vec![0, 101];
                rtp_attrs.push("a=rtpmap:0 PCMU/8000\r\n".to_string());
                rtp_attrs.push("a=rtpmap:101 telephone-event/8000\r\n".to_string());
                rtp_attrs.push("a=fmtp:101 0-15\r\n".to_string());
            }

            // Build media line with RTP/AVP
            let format_str = rtp_formats
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(" ");
            rtp_sdp.push_str(&format!(
                "m=audio {} RTP/AVP {}\r\n",
                section.port, format_str
            ));

            // Add collected attributes
            for attr in rtp_attrs {
                rtp_sdp.push_str(&attr);
            }
        }

        Ok(rtp_sdp)
    }

    /// Convert RTP SDP to WebRTC SDP
    ///
    /// Changes:
    /// - Protocol: RTP/AVP → UDP/TLS/RTP/SAVPF  
    /// - Add: fingerprint, ice-ufrag, ice-pwd, setup, rtcp-mux
    /// - Keep: compatible codecs
    pub fn rtp_to_webrtc(rtp_sdp: &str, fingerprint: &str, ice_ufrag: &str, ice_pwd: &str) -> Result<String> {
        let parsed = SessionDescription::parse(SdpType::Offer, rtp_sdp)
            .or_else(|_| SessionDescription::parse(SdpType::Answer, rtp_sdp))
            .map_err(|e| anyhow!("Failed to parse RTP SDP: {}", e))?;

        let mut webrtc_sdp = String::new();

        // Add session header
        webrtc_sdp.push_str("v=0\r\n");
        webrtc_sdp.push_str(&format!(
            "o=- {} {} IN IP4 {}\r\n",
            parsed.session.origin.session_id,
            parsed.session.origin.session_version,
            parsed.session.origin.unicast_address
        ));
        webrtc_sdp.push_str("s=RustPBX Bridge\r\n");
        webrtc_sdp.push_str(&format!("c=IN IP4 {}\r\n", parsed.session.origin.unicast_address));
        webrtc_sdp.push_str("t=0 0\r\n");

        // Transform media sections
        for section in &parsed.media_sections {
            if section.kind != MediaKind::Audio {
                continue;
            }

            let mut webrtc_formats = Vec::new();
            let mut webrtc_attrs = Vec::new();

            // Collect formats
            for format in &section.formats {
                if let Ok(pt) = format.parse::<u8>() {
                    webrtc_formats.push(pt);
                }
            }

            // Process attributes
            for attr in &section.attributes {
                if attr.key == "rtpmap" {
                    if let Some(ref value) = attr.value {
                        webrtc_attrs.push(format!("a=rtpmap:{}\r\n", value));
                    }
                } else if attr.key == "fmtp" {
                    if let Some(ref value) = attr.value {
                        webrtc_attrs.push(format!("a=fmtp:{}\r\n", value));
                    }
                } else if attr.key == "sendrecv"
                    || attr.key == "sendonly"
                    || attr.key == "recvonly"
                    || attr.key == "inactive"
                {
                    webrtc_attrs.push(format!("a={}\r\n", attr.key));
                }
            }

            // Add WebRTC-specific attributes
            webrtc_attrs.push(format!("a=fingerprint:sha-256 {}\r\n", fingerprint));
            webrtc_attrs.push("a=setup:actpass\r\n".to_string());
            webrtc_attrs.push(format!("a=ice-ufrag:{}\r\n", ice_ufrag));
            webrtc_attrs.push(format!("a=ice-pwd:{}\r\n", ice_pwd));
            webrtc_attrs.push("a=rtcp-mux\r\n".to_string());
            webrtc_attrs.push("a=mid:0\r\n".to_string());

            // Build media line with SAVPF
            let format_str = webrtc_formats
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(" ");
            webrtc_sdp.push_str(&format!(
                "m=audio {} UDP/TLS/RTP/SAVPF {}\r\n",
                section.port, format_str
            ));

            // Add attributes
            for attr in webrtc_attrs {
                webrtc_sdp.push_str(&attr);
            }
        }

        Ok(webrtc_sdp)
    }

    /// Check if SDP needs bridging (different formats between caller and callee)
    pub fn needs_bridging(caller_sdp: &str, callee_sdp: &str) -> bool {
        let caller_format = detect_sdp_format(caller_sdp);
        let callee_format = detect_sdp_format(callee_sdp);
        caller_format != callee_format
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_webrtc_sdp() {
        let webrtc_sdp = "v=0\r\n\
            o=- 123 123 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            m=audio 1234 UDP/TLS/RTP/SAVPF 111\r\n\
            a=fingerprint:sha-256 AA:BB\r\n";
        assert_eq!(detect_sdp_format(webrtc_sdp), SdpFormat::WebRtc);
    }

    #[test]
    fn test_detect_rtp_sdp() {
        let rtp_sdp = "v=0\r\n\
            o=- 123 123 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            m=audio 1234 RTP/AVP 0\r\n";
        assert_eq!(detect_sdp_format(rtp_sdp), SdpFormat::Rtp);
    }

    #[test]
    fn test_webrtc_to_rtp_conversion() {
        let webrtc_sdp = "v=0\r\n\
o=- 123456 123456 IN IP4 127.0.0.1\r\n\
s=-\r\n\
c=IN IP4 127.0.0.1\r\n\
t=0 0\r\n\
m=audio 12345 UDP/TLS/RTP/SAVPF 111 101\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=fingerprint:sha-256 AA:BB:CC:DD\r\n\
a=setup:actpass\r\n\
a=ice-ufrag:abcd\r\n\
a=ice-pwd:xyz\r\n\
a=rtcp-mux\r\n\
a=sendrecv\r\n";

        let rtp_sdp = SdpBridge::webrtc_to_rtp(webrtc_sdp).unwrap();
        
        // Should use RTP/AVP
        assert!(rtp_sdp.contains("RTP/AVP"));
        assert!(!rtp_sdp.contains("SAVPF"));
        
        // Should remove WebRTC-specific attributes
        assert!(!rtp_sdp.contains("fingerprint"));
        assert!(!rtp_sdp.contains("ice-ufrag"));
        assert!(!rtp_sdp.contains("rtcp-mux"));
        
        // Should keep telephone-event
        assert!(rtp_sdp.contains("telephone-event"));
    }

    #[test]
    fn test_rtp_to_webrtc_conversion() {
        let rtp_sdp = "v=0\r\n\
o=- 123456 123456 IN IP4 127.0.0.1\r\n\
s=-\r\n\
c=IN IP4 127.0.0.1\r\n\
t=0 0\r\n\
m=audio 54321 RTP/AVP 0 101\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=sendrecv\r\n";

        let webrtc_sdp = SdpBridge::rtp_to_webrtc(
            rtp_sdp,
            "AA:BB:CC:DD:EE:FF",
            "ufrag123",
            "pwd456"
        ).unwrap();
        
        // Should use SAVPF
        assert!(webrtc_sdp.contains("UDP/TLS/RTP/SAVPF"));
        
        // Should add WebRTC-specific attributes
        assert!(webrtc_sdp.contains("fingerprint:sha-256 AA:BB:CC:DD:EE:FF"));
        assert!(webrtc_sdp.contains("ice-ufrag:ufrag123"));
        assert!(webrtc_sdp.contains("ice-pwd:pwd456"));
        assert!(webrtc_sdp.contains("rtcp-mux"));
        assert!(webrtc_sdp.contains("setup:actpass"));
        
        // Should keep PCMU
        assert!(webrtc_sdp.contains("PCMU/8000"));
    }
}
