/// Key used to deduplicate repeated telephone-event packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DtmfEventKey {
    digit_code: u8,
    rtp_timestamp: u32,
}

/// Stateful deduplicator for RFC 2833 telephone-event packets.
///
/// The same DTMF digit may arrive in multiple RTP packets with different
/// timestamps (start, continue, end).  This detector emits a `char` only
/// once per (digit_code, rtp_timestamp) pair so that duplicate RTP packets
/// (e.g. from a retransmission or from both the recorder tap and the
/// forwarding path) do not produce duplicate digits.
#[derive(Debug, Default)]
pub struct DtmfDetector {
    last_event: Option<DtmfEventKey>,
}

impl DtmfDetector {
    pub fn observe(&mut self, payload: &[u8], rtp_timestamp: u32) -> Option<char> {
        if payload.len() < 4 {
            return None;
        }

        // A digit press is carried by a start frame followed by an end frame
        // (RFC 4733). Emitting on every packet would deliver each digit
        // multiple times (sipbot/phones send start + end), which corrupts
        // DTMF collection in call apps (e.g. check-voicemail extension/PIN).
        // Only emit when the end-of-event bit is set, i.e. once per press.
        if payload[1] & 0x80 == 0 {
            return None;
        }

        let digit_code = payload[0];
        let digit = crate::telephone_event::dtmf_code_to_char(digit_code)?;

        let event = DtmfEventKey {
            digit_code,
            rtp_timestamp,
        };

        if self.last_event == Some(event) {
            return None;
        }

        self.last_event = Some(event);
        Some(digit)
    }
}
