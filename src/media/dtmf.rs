use std::sync::atomic::{AtomicU64, AtomicU8};
// DTMF events as per RFC 4733
const DTMF_EVENT_0: u8 = 0;
const DTMF_EVENT_1: u8 = 1;
const DTMF_EVENT_2: u8 = 2;
const DTMF_EVENT_3: u8 = 3;
const DTMF_EVENT_4: u8 = 4;
const DTMF_EVENT_5: u8 = 5;
const DTMF_EVENT_6: u8 = 6;
const DTMF_EVENT_7: u8 = 7;
const DTMF_EVENT_8: u8 = 8;
const DTMF_EVENT_9: u8 = 9;
const DTMF_EVENT_STAR: u8 = 10;
const DTMF_EVENT_POUND: u8 = 11;
const DTMF_EVENT_A: u8 = 12;
const DTMF_EVENT_B: u8 = 13;
const DTMF_EVENT_C: u8 = 14;
const DTMF_EVENT_D: u8 = 15;

pub struct DtmfDetector {
    // Track the last seen event to avoid repeated events
    last_event: AtomicU8,
    last_duration: AtomicU64,
}

struct DtmfPayload {
    event: u8,     // 8bits
    is_end: bool,  // 1bit
    _reserved: u8, // 1bits
    _volume: u8,   // 6bits
    duration: u16, // 16bits
}

impl DtmfPayload {
    fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < 4 {
            return None;
        }

        let event = payload[0];
        if event > DTMF_EVENT_D {
            return None;
        }

        //     0                   1                   2                   3
        //     0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
        //    +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
        //    |     event     |E|R| volume    |          duration             |
        //    +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
        //                  Figure 1: Payload Format for Named Events

        // Second byte: End bit (E) is the most significant bit (bit 7)
        let is_end = (payload[1] & 0b1000_0000) != 0;
        // Reserved bit (R) is the second most significant bit (bit 6)
        let reserved = payload[1] & 0b0100_0000;
        // Volume is the 6 least significant bits (0-5)
        let volume = payload[1] & 0b0011_1111;

        // Duration is a 16-bit value spanning bytes 2 and 3
        let duration_high = payload[2] as u16;
        let duration_low = payload[3] as u16;
        let duration = (duration_high << 8) | duration_low;

        Some(Self {
            event,
            is_end,
            _reserved: reserved,
            _volume: volume,
            duration,
        })
    }
}

impl DtmfDetector {
    pub fn new() -> Self {
        Self {
            last_event: AtomicU8::new(0),
            last_duration: AtomicU64::new(0),
        }
    }

    // Detect DTMF events from RTP payload as specified in RFC 4733
    pub fn detect_rtp(&self, payload_type: u8, payload: &[u8]) -> Option<String> {
        // RFC 4733 defines DTMF events with payload types 96-127 (dynamic)
        // However, we'll be more lenient and just check if the payload has the right format
        if payload.len() < 4 {
            return None;
        }

        // Generally, telephone-event payload type is in dynamic range 96-127
        if payload_type < 96 || payload_type > 127 {
            return None;
        }

        // Parse the DTMF payload
        let dtmf_payload = DtmfPayload::parse(payload)?;

        // Only report end packets to avoid duplicates
        if !dtmf_payload.is_end {
            return None;
        }
        // Get current duration
        let current_duration = crate::get_timestamp();
        let last_duration = self
            .last_duration
            .load(std::sync::atomic::Ordering::Relaxed);
        let last_event = self.last_event.load(std::sync::atomic::Ordering::Relaxed);

        // Check if this is a duplicate event (same event with similar duration)
        if dtmf_payload.event == last_event
            && (current_duration <= last_duration || current_duration - last_duration < 100)
        {
            return None;
        }

        // Update last event and duration
        self.last_event
            .store(dtmf_payload.event, std::sync::atomic::Ordering::Relaxed);
        self.last_duration.store(
            current_duration + dtmf_payload.duration as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        Some(
            match dtmf_payload.event {
                DTMF_EVENT_0 => "0",
                DTMF_EVENT_1 => "1",
                DTMF_EVENT_2 => "2",
                DTMF_EVENT_3 => "3",
                DTMF_EVENT_4 => "4",
                DTMF_EVENT_5 => "5",
                DTMF_EVENT_6 => "6",
                DTMF_EVENT_7 => "7",
                DTMF_EVENT_8 => "8",
                DTMF_EVENT_9 => "9",
                DTMF_EVENT_STAR => "*",
                DTMF_EVENT_POUND => "#",
                DTMF_EVENT_A => "A",
                DTMF_EVENT_B => "B",
                DTMF_EVENT_C => "C",
                DTMF_EVENT_D => "D",
                _ => return None, // Invalid event
            }
            .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dtmf_payload_parse() {
        // Valid DTMF payload for digit "1" with end bit set
        // Event: 1, End bit: 1, Reserved: 0, Volume: 10, Duration: 160
        let payload = [1, 0x8A, 0, 160]; // 0x8A = 10001010 (end=1, reserved=0, volume=10)

        let dtmf = DtmfPayload::parse(&payload).unwrap();
        assert_eq!(dtmf.event, 1);
        assert_eq!(dtmf.is_end, true);
        assert_eq!(dtmf._reserved, 0);
        assert_eq!(dtmf._volume, 10); // 10 = 001010 binary
        assert_eq!(dtmf.duration, 160);

        // Test payload with end bit not set
        let payload = [2, 0x00, 0, 160]; // 0x00 = 00000000 (end=0, reserved=0, volume=0)

        let dtmf = DtmfPayload::parse(&payload).unwrap();
        assert_eq!(dtmf.event, 2);
        assert_eq!(dtmf.is_end, false);
        assert_eq!(dtmf._volume, 0);

        // Invalid event code
        let payload = [20, 0x80, 10, 100]; // 20 > DTMF_EVENT_D
        assert!(DtmfPayload::parse(&payload).is_none());

        // Too short payload
        let payload = [1, 0x80, 10]; // Missing duration byte
        assert!(DtmfPayload::parse(&payload).is_none());

        // Test the specific case [2, 138, 3, 32]
        let payload = [2, 138, 3, 32];
        // 138 decimal = 10001010 binary
        // End bit (bit 7) = 1
        // Reserved bit (bit 6) = 0
        // Volume (bits 0-5) = 001010 = 10

        let dtmf = DtmfPayload::parse(&payload).unwrap();
        assert_eq!(dtmf.event, 2); // DTMF digit "2"
        assert_eq!(dtmf.is_end, true); // End bit is set
        assert_eq!(dtmf._reserved, 0); // Reserved bit is 0
        assert_eq!(dtmf._volume, 10); // Volume is 10
        assert_eq!(dtmf.duration, 800); // Duration is 3 * 256 + 32 = 800
    }

    #[test]
    fn test_dtmf_detection() {
        let detector = DtmfDetector::new();

        // Test basic detection
        {
            // Valid DTMF payload for digit "5" with end bit set
            let payload = [DTMF_EVENT_5, 0x80, 10, 100];

            // Use payload_type 101 (typical for telephone-event)
            let digit = detector.detect_rtp(101, &payload);
            assert_eq!(digit, Some("5".to_string()));

            // Should reject payloads with invalid payload type
            let digit = detector.detect_rtp(0, &payload);
            assert_eq!(digit, None);

            // Should reject payloads with end bit not set
            let payload = [DTMF_EVENT_5, 0x00, 10, 100];
            let digit = detector.detect_rtp(101, &payload);
            assert_eq!(digit, None);
        }

        // Test duplicate detection
        {
            let detector = DtmfDetector::new(); // Use a fresh detector

            // First event
            let payload1 = [DTMF_EVENT_5, 0x80, 0, 100]; // Duration 100
            let digit1 = detector.detect_rtp(101, &payload1);
            assert_eq!(digit1, Some("5".to_string()));

            // Similar duration - should be rejected as duplicate
            let payload2 = [DTMF_EVENT_5, 0x80, 0, 100]; // Duration 150 (similar)
            let digit2 = detector.detect_rtp(101, &payload2);
            assert_eq!(digit2, None);
            std::thread::sleep(std::time::Duration::from_millis(200));
            // Much larger duration - should be detected as new event
            let payload3 = [DTMF_EVENT_5, 0x80, 0, 100]; // Much larger duration
            let digit3 = detector.detect_rtp(101, &payload3);
            assert_eq!(digit3, Some("5".to_string()));

            // Different event - should be detected
            let payload4 = [DTMF_EVENT_6, 0x80, 1, 8]; // Event 6 ("6" key)
            let digit4 = detector.detect_rtp(101, &payload4);
            assert_eq!(digit4, Some("6".to_string()));
        }
    }
}
