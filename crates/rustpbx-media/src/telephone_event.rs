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

/// Largest duration in 8 kHz units that still fits after conversion to 48 kHz.
/// `10_922 * 6 = 65_532`, which is below `u16::MAX`.
pub const DTMF_CANONICAL_MAX_DURATION: u16 = 10_922;

fn duration_to_8k(duration: u16, source_clock_rate: u32) -> Option<u16> {
    let duration = match source_clock_rate {
        8000 => duration,
        48000 => duration / 6,
        _ => return None,
    };
    Some(duration.min(DTMF_CANONICAL_MAX_DURATION))
}

fn duration_from_8k(duration: u16, target_clock_rate: u32) -> Option<u16> {
    let duration = duration.min(DTMF_CANONICAL_MAX_DURATION);
    match target_clock_rate {
        8000 => Some(duration),
        48000 => Some(duration * 6),
        _ => None,
    }
}

/// Preserve the event, end bit and volume, changing only the RFC 4733
/// cumulative duration between the supported 8 kHz and 48 kHz clocks.
pub fn map_telephone_event_duration(
    payload: &[u8],
    source_clock_rate: u32,
    target_clock_rate: u32,
) -> Option<Vec<u8>> {
    if payload.len() < 4 {
        return None;
    }
    let source_duration = u16::from_be_bytes([payload[2], payload[3]]);
    let duration_8k = duration_to_8k(source_duration, source_clock_rate)?;
    let target_duration = duration_from_8k(duration_8k, target_clock_rate)?.to_be_bytes();
    let mut mapped = payload.to_vec();
    mapped[2] = target_duration[0];
    mapped[3] = target_duration[1];
    Some(mapped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telephone_event_duration_maps_48k_to_8k() {
        let mapped = map_telephone_event_duration(&[2, 0x8a, 0x12, 0xc0], 48000, 8000)
            .expect("supported clocks");
        assert_eq!(&mapped[..2], &[2, 0x8a]);
        assert_eq!(u16::from_be_bytes([mapped[2], mapped[3]]), 800);
    }

    #[test]
    fn telephone_event_duration_caps_before_48k_multiplication() {
        let mapped = map_telephone_event_duration(&[5, 0x87, 0xff, 0xff], 8000, 48000)
            .expect("supported clocks");
        assert_eq!(u16::from_be_bytes([mapped[2], mapped[3]]), 65_532);
    }
}
