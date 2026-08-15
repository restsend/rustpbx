//! RFC 4733 telephone-event (DTMF) synthesis and parsing.

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
/// frame? Used so DTMF digits are never decoded as audio (which would destroy
/// them): exactly 4 bytes, first byte is a valid digit (0..=15), and the
/// "End of event" + "Reserved" bits in byte 1 are not both set in a way that
/// real audio would produce.
pub fn looks_like_dtmf_payload(payload: &[u8]) -> bool {
    if payload.len() != 4 {
        return false;
    }
    matches!(payload[0], 0..=15) && (payload[1] & 0x40) == 0
}

/// One parsed RFC 4733 telephone-event frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DtmfEvent {
    pub digit: char,
    /// Volume in dBm0 (negative; stored as the raw u8).
    pub volume: u8,
    /// Duration in event-clock units (typically 8 kHz).
    pub duration_units: u16,
    /// Whether the End-of-event bit is set.
    pub end_bit: bool,
}

/// Parse a 4-byte RFC 4733 telephone-event payload.
pub fn parse_dtmf_event(payload: &[u8]) -> Option<DtmfEvent> {
    if payload.len() < 4 {
        return None;
    }
    let digit = dtmf_code_to_char(payload[0])?;
    Some(DtmfEvent {
        digit,
        volume: payload[1] & 0x3F,
        duration_units: u16::from_be_bytes([payload[2], payload[3]]),
        end_bit: (payload[1] & 0x80) != 0,
    })
}

/// Convert an event duration (measured in the event clock) to samples at the
/// target (codec) rate.
pub fn dtmf_duration_samples(duration_units: u16, event_clock: u32, target_rate: u32) -> u32 {
    if event_clock == 0 {
        return 0;
    }
    (duration_units as u64 * target_rate as u64 / event_clock as u64) as u32
}

/// Dual-tone DTMF synthesizer (standard 697–941 Hz × 1209–1633 Hz matrix).
pub struct DtmfGenerator {
    sample_rate: u32,
}

impl DtmfGenerator {
    pub fn new(sample_rate: u32) -> Self {
        Self { sample_rate }
    }

    pub fn generate(&self, digit: char, duration_ms: u32) -> Vec<i16> {
        let num_samples = (self.sample_rate as f32 * (duration_ms as f32 / 1000.0)) as usize;
        self.generate_samples(digit, num_samples)
    }

    pub fn generate_samples(&self, digit: char, num_samples: usize) -> Vec<i16> {
        let freqs = match digit {
            '1' => (697.0, 1209.0),
            '2' => (697.0, 1336.0),
            '3' => (697.0, 1477.0),
            '4' => (770.0, 1209.0),
            '5' => (770.0, 1336.0),
            '6' => (770.0, 1477.0),
            '7' => (852.0, 1209.0),
            '8' => (852.0, 1336.0),
            '9' => (852.0, 1477.0),
            '*' => (941.0, 1209.0),
            '0' => (941.0, 1336.0),
            '#' => (941.0, 1477.0),
            'A' => (697.0, 1633.0),
            'B' => (770.0, 1633.0),
            'C' => (852.0, 1633.0),
            'D' => (941.0, 1633.0),
            _ => return Vec::new(),
        };
        let mut samples = Vec::with_capacity(num_samples);
        for i in 0..num_samples {
            let t = i as f32 / self.sample_rate as f32;
            let s1 = (2.0 * std::f32::consts::PI * freqs.0 * t).sin();
            let s2 = (2.0 * std::f32::consts::PI * freqs.1 * t).sin();
            let s = (s1 + s2) / 2.0;
            samples.push((s * 32767.0) as i16);
        }
        samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_char_roundtrip() {
        for code in 0u8..=15 {
            let c = dtmf_code_to_char(code).unwrap();
            assert_eq!(dtmf_char_to_code(c), Some(code));
        }
        assert_eq!(dtmf_char_to_code('e'), None);
        assert_eq!(dtmf_code_to_char(16), None);
    }

    #[test]
    fn payload_heuristic() {
        assert!(looks_like_dtmf_payload(&[1, 0x0A, 0x00, 0xA0]));
        // End-bit set is still DTMF.
        assert!(looks_like_dtmf_payload(&[1, 0x8A, 0x00, 0xA0]));
        // Reserved bit set → not DTMF.
        assert!(!looks_like_dtmf_payload(&[1, 0x4A, 0x00, 0xA0]));
        // Wrong length.
        assert!(!looks_like_dtmf_payload(&[1, 0x0A, 0x00]));
        // Invalid digit code.
        assert!(!looks_like_dtmf_payload(&[0xFF, 0x0A, 0x00, 0xA0]));
    }

    #[test]
    fn parse_event_fields() {
        let ev = parse_dtmf_event(&[5, 0x8A, 0x01, 0x40]).unwrap();
        assert_eq!(ev.digit, '5');
        assert!(ev.end_bit);
        assert_eq!(ev.duration_units, 0x0140);
        assert_eq!(ev.volume, 0x0A);
        assert!(parse_dtmf_event(&[0x10, 0x00, 0x00, 0x00]).is_none());
        assert!(parse_dtmf_event(&[]).is_none());
    }

    #[test]
    fn duration_conversion() {
        // 160 units @8kHz → 160 samples @8kHz.
        assert_eq!(dtmf_duration_samples(160, 8000, 8000), 160);
        // 160 units @8kHz → 320 samples @16kHz.
        assert_eq!(dtmf_duration_samples(160, 8000, 16000), 320);
        assert_eq!(dtmf_duration_samples(100, 0, 8000), 0);
    }

    #[test]
    fn generator_frequencies() {
        let g = DtmfGenerator::new(8000);
        let s = g.generate('1', 100);
        assert_eq!(s.len(), 800);
        // Energy at both row (697) and column (1209) frequencies, none at
        // 1000 Hz (crude Goertzel-style check with dot products).
        let energy = |f: f32| {
            s.iter()
                .enumerate()
                .map(|(i, &v)| {
                    let t = i as f32 / 8000.0;
                    v as f32 * (2.0 * std::f32::consts::PI * f * t).sin()
                })
                .sum::<f32>()
                .abs()
                / s.len() as f32
        };
        assert!(energy(697.0) > 1000.0, "row tone present");
        assert!(energy(1209.0) > 1000.0, "column tone present");
        assert!(energy(1000.0) < 100.0, "no stray tone");
        // Unknown digit → empty.
        assert!(g.generate('e', 100).is_empty());
    }
}
