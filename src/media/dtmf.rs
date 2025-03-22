use std::collections::HashMap;
use std::f32::consts::PI;

// DTMF frequencies according to ITU-T Q.23
const DTMF_FREQUENCIES: [(f32, f32); 16] = [
    (697.0, 1209.0), // 1
    (697.0, 1336.0), // 2
    (697.0, 1477.0), // 3
    (770.0, 1209.0), // 4
    (770.0, 1336.0), // 5
    (770.0, 1477.0), // 6
    (852.0, 1209.0), // 7
    (852.0, 1336.0), // 8
    (852.0, 1477.0), // 9
    (941.0, 1336.0), // 0
    (941.0, 1209.0), // *
    (941.0, 1477.0), // #
    (697.0, 1633.0), // A
    (770.0, 1633.0), // B
    (852.0, 1633.0), // C
    (941.0, 1633.0), // D
];

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

pub struct DTMFDetector {
    // Track the last seen event to avoid repeated events
    last_event: HashMap<String, u8>,
}

impl DTMFDetector {
    pub fn new() -> Self {
        Self {
            last_event: HashMap::new(),
        }
    }

    // Detect DTMF events from RTP payload as specified in RFC 4733
    pub fn detect_rtp(
        &mut self,
        track_id: &str,
        payload_type: u8,
        payload: &[u8],
    ) -> Option<String> {
        // RFC 4733 defines DTMF events with payload types 96-127 (dynamic)
        // However, we'll be more lenient and just check if the payload has the right format
        if payload.len() < 4 {
            return None;
        }
        if payload_type < 96 || payload_type > 127 {
            return None;
        }
        // Extract the event code (first byte)
        let event = payload[0];

        // Check if this is a valid DTMF event (0-15)
        if event > DTMF_EVENT_D {
            return None;
        }

        // Check if this is the same as the last event for this track
        if let Some(&last) = self.last_event.get(track_id) {
            if last == event {
                return None; // Don't repeat the same event
            }
        }

        // Store this event as the last seen for this track
        self.last_event.insert(track_id.to_string(), event);

        // Convert the event code to a DTMF character
        Some(
            match event {
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
                _ => unreachable!(), // We already checked if event <= 15
            }
            .to_string(),
        )
    }

    // Generate DTMF tone samples for a given character
    pub fn generate_tone(digit: &str, sample_rate: u32, duration_ms: u32) -> Vec<i16> {
        let digit_index = match digit {
            "1" => 0,
            "2" => 1,
            "3" => 2,
            "4" => 3,
            "5" => 4,
            "6" => 5,
            "7" => 6,
            "8" => 7,
            "9" => 8,
            "0" => 9,
            "*" => 10,
            "#" => 11,
            "A" => 12,
            "B" => 13,
            "C" => 14,
            "D" => 15,
            _ => return vec![], // Not a valid DTMF digit
        };

        let (freq1, freq2) = DTMF_FREQUENCIES[digit_index];
        let samples = (sample_rate as f32 * duration_ms as f32 / 1000.0) as usize;
        let mut output = Vec::with_capacity(samples);

        for i in 0..samples {
            let t = i as f32 / sample_rate as f32;

            // Generate the dual tone with 3dB attenuation to avoid clipping
            let sample = 0.35 * (2.0 * PI * freq1 * t).sin() + 0.35 * (2.0 * PI * freq2 * t).sin();

            // Apply envelope to avoid clicks (quick attack, longer release)
            let envelope = if i < 100 {
                i as f32 / 100.0
            } else if i > samples - 100 {
                (samples - i) as f32 / 100.0
            } else {
                1.0
            };

            // Convert to i16
            let value = (sample * envelope * 32767.0) as i16;
            output.push(value);
        }

        output
    }
}
