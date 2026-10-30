//! Extract Call-ID from a UDP datagram for routing.

pub const MAX_SCAN_BYTES: usize = 4096;

pub fn extract_call_id(datagram: &[u8]) -> Option<String> {
    let limit = datagram.len().min(MAX_SCAN_BYTES);
    let buf = &datagram[..limit];
    let header_end = find_header_end(buf)?;
    let headers = &buf[..header_end];
    let mut pos = 0;
    while pos < headers.len() {
        let eol = memchr(headers, b'\n', pos).unwrap_or(headers.len());
        let line = &headers[pos..eol];
        let trimmed = trim_cr(line);
        if let Some(v) = parse_call_id(trimmed) { return Some(v); }
        pos = eol + 1;
    }
    None
}

fn trim_cr(line: &[u8]) -> &[u8] {
    if line.last() == Some(&b'\r') { &line[..line.len()-1] } else { line }
}

fn parse_call_id(line: &[u8]) -> Option<String> {
    let lower: Vec<u8> = line.iter().map(|b| b.to_ascii_lowercase()).collect();
    for name in [&b"call-id"[..], &b"i"[..]] {
        let colon = name.len();
        if lower.len() <= colon + 1 || lower[..colon] != *name { continue; }
        if lower[colon] != b':' { continue; }
        if name == b"i" && lower.get(colon+1).is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'-') { continue; }
        let mut rest = &line[colon+1..];
        while rest.first().is_some_and(|b| *b == b' ' || *b == b'\t') { rest = &rest[1..]; }
        return Some(String::from_utf8_lossy(rest).to_string());
    }
    None
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    for i in 0..buf.len().saturating_sub(1) {
        if buf[i] == b'\n' {
            if buf[i+1] == b'\n' { return Some(i+1); }
            if i+2 < buf.len() && buf[i+1] == b'\r' && buf[i+2] == b'\n' { return Some(i+1); }
        }
    }
    None
}

fn memchr(hay: &[u8], needle: u8, from: usize) -> Option<usize> {
    hay[from..].iter().position(|&b| b == needle).map(|p| p + from)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extracts_call_id() {
        assert_eq!(extract_call_id(b"BYE sip:x SIP/2.0\r\nCall-ID: abc\r\n\r\n"), Some("abc".into()));
    }
    #[test]
    fn garbage_returns_none() {
        assert_eq!(extract_call_id(b"\x00\xff"), None);
    }
}
