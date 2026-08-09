/// RFC 4733 telephone-event code to character mapping.
pub fn dtmf_code_to_char(code: u8) -> Option<char> {
    rustpbx_sipflow::wav_utils::dtmf_code_to_char(code)
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
/// frame? Used so DTMF digits are never decoded as audio (which would destroy
/// them): exactly 4 bytes, first byte is a valid digit (0..=15), and the
/// "End of event" + "Reserved" bits in byte 1 are not both set in a way that
/// real audio would produce.
pub fn looks_like_dtmf_payload(payload: &[u8]) -> bool {
    rustpbx_sipflow::wav_utils::looks_like_dtmf_payload(payload)
}

/// Build an RFC 4733 telephone-event payload for one DTMF digit.
///
/// Format: `[event_code, end|volume, duration_hi, duration_lo]` (4 bytes).
/// The start packet carries the digit code with the E bit clear; the end
/// packet sets the E bit and records the total event duration in 8 kHz
/// timestamp units (160 = 20 ms).
pub fn telephone_event_payload(code: u8, end: bool, duration: u16) -> Vec<u8> {
    vec![
        code & 0x0F,
        (if end { 0x80 } else { 0x00 }) | 10,
        (duration >> 8) as u8,
        (duration & 0xFF) as u8,
    ]
}
