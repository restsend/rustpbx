/// Extract RTP address (IP:Port) from SDP using simple string parsing
pub fn extract_rtp_addr(sdp: &str) -> Option<String> {
    let mut ip = None;
    let mut port = None;

    for line in sdp.lines() {
        // Connection line: c=IN IP4 192.168.1.100
        if line.starts_with("c=IN IP4 ") || line.starts_with("c=IN IP6 ") {
            ip = line.split_whitespace().last().map(|s| s.to_string());
        }
        // Media line: m=audio 20000 RTP/AVP 0
        if line.starts_with("m=audio ") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                port = Some(parts[1].to_string());
            }
        }
    }

    match (ip, port) {
        (Some(i), Some(p)) if p != "0" => Some(format!("{}:{}", i, p)),
        _ => None,
    }
}

/// Extract Call-ID from SIP message
pub fn extract_call_id(message: &str) -> Option<String> {
    for line in message.lines() {
        if line.starts_with("Call-ID:") || line.starts_with("i:") {
            return line
                .split(':')
                .nth(1)
                .map(|s| s.trim().to_string());
        }
    }
    None
}

/// Extract SDP body from SIP message
pub fn extract_sdp(message: &str) -> Option<String> {
    // Find empty line separating headers from body
    let parts: Vec<&str> = message.split("\r\n\r\n").collect();
    if parts.len() < 2 {
        return None;
    }

    let body = parts[1];
    if body.contains("v=0") && body.contains("m=audio") {
        Some(body.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_rtp_addr() {
        let sdp = r#"v=0
o=- 123 456 IN IP4 192.168.1.100
s=-
c=IN IP4 192.168.1.100
t=0 0
m=audio 20000 RTP/AVP 0 8
a=rtpmap:0 PCMU/8000
a=rtpmap:8 PCMA/8000"#;

        let addr = extract_rtp_addr(sdp);
        assert_eq!(addr, Some("192.168.1.100:20000".to_string()));
    }

    #[test]
    fn test_extract_rtp_addr_rejected() {
        let sdp = r#"v=0
o=- 123 456 IN IP4 192.168.1.100
s=-
c=IN IP4 0.0.0.0
t=0 0
m=audio 0 RTP/AVP 0"#;

        let addr = extract_rtp_addr(sdp);
        assert_eq!(addr, None);
    }

    #[test]
    fn test_extract_call_id() {
        let message = "INVITE sip:bob@example.com SIP/2.0\r\nCall-ID: abc123\r\n\r\n";
        assert_eq!(extract_call_id(message), Some("abc123".to_string()));
    }

    #[test]
    fn test_extract_sdp() {
        let message = "INVITE sip:bob@example.com SIP/2.0\r\nCall-ID: abc\r\n\r\nv=0\r\nm=audio 5000 RTP/AVP 0";
        let sdp = extract_sdp(message);
        assert!(sdp.is_some());
        assert!(sdp.unwrap().contains("v=0"));
    }
}
