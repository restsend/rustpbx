/// RFC 4733 telephone-event code to character mapping.
pub fn dtmf_code_to_char(code: u8) -> Option<char> {
    match code {
        0..=9 => Some((b'0' + code) as char),
        10 => Some('*'),
        11 => Some('#'),
        12..=15 => Some((b'A' + (code - 12)) as char),
        _ => None,
    }
}

/// RFC 4733 character to telephone-event code mapping.
pub fn dtmf_char_to_code(c: char) -> Option<u8> {
    match c {
        '0'..='9' => Some(c as u8 - b'0'),
        '*' => Some(10),
        '#' => Some(11),
        'A' | 'a' => Some(12),
        'B' | 'b' => Some(13),
        'C' | 'c' => Some(14),
        'D' | 'd' => Some(15),
        _ => None,
    }
}

/// Heuristic check: does this RTP payload look like an RFC 4733 telephone-event
/// frame? Used by transcoder bypass logic so DTMF digits are never decoded as
/// audio (which would destroy them).
///
/// The check mirrors `sipflow::wav_utils::looks_like_dtmf_payload`:
/// exactly 4 bytes, first byte is a valid digit (0..=15), and the
/// "End of event" + "Reserved" bits in byte 1 are not both set in a way that
/// real audio would produce.
pub fn looks_like_dtmf_payload(payload: &[u8]) -> bool {
    if payload.len() != 4 {
        return false;
    }
    matches!(payload[0], 0..=15) && (payload[1] & 0x40) == 0
}
