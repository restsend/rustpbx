use std::sync::Arc;

use crate::media::bridge::BridgeEndpoint;

/// Callback invoked when a DTMF digit is detected from a bridge endpoint.
pub type DtmfHandler = Arc<dyn Fn(char) + Send + Sync + 'static>;

/// Per-endpoint DTMF sink — which payload types carry telephone-events
/// and where to forward detected digits.
#[derive(Clone)]
pub struct DtmfSink {
    pub endpoint: BridgeEndpoint,
    pub payload_types: Vec<u8>,
    pub handler: DtmfHandler,
}

/// Maps a source telephone-event payload type to a target payload type,
/// adjusting clock rate for the other side of the bridge (e.g. Opus 48kHz
/// → PCMU 8kHz).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadMapping {
    pub source_pt: u8,
    pub target_pt: u8,
    pub source_clock_rate: u32,
    pub target_clock_rate: u32,
}

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

        let digit_code = payload[0];
        let digit = crate::media::telephone_event::dtmf_code_to_char(digit_code)?;

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
