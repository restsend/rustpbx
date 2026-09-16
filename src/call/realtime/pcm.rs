//! Raw PCM16-LE WebSocket bridge protocol — the same wire format the
//! `voip_bridge:` transfer target speaks, so any self-hosted realtime
//! server built for `voip_bridge` works with the realtime app unchanged.
//!
//! Wire format:
//! - Uplink audio: binary frames, headerless PCM16 mono LE at the
//!   configured sample rate (default 8000 Hz).
//! - Downlink audio: binary frames, same encoding.
//! - DTMF: JSON text `{"type":"dtmf","digit":"5"}` both directions.

use super::{
    DownlinkEvent, RealtimeParams, RealtimeProtocol, RealtimeProtocolKind, UplinkMessage,
    bytes_to_pcm, pcm_to_bytes,
};

pub struct PcmBridge;

impl RealtimeProtocol for PcmBridge {
    fn kind(&self) -> RealtimeProtocolKind {
        RealtimeProtocolKind::PcmBridge
    }

    fn sample_rate(&self, params: &RealtimeParams) -> u32 {
        params.sample_rate.unwrap_or(8000)
    }

    fn connect_url(&self, params: &RealtimeParams) -> String {
        params.url.clone()
    }

    fn upgrade_headers(&self, params: &RealtimeParams) -> Vec<(String, String)> {
        params.extra_headers.clone()
    }

    fn session_open_messages(&self, _params: &RealtimeParams) -> Vec<UplinkMessage> {
        Vec::new()
    }

    fn encode_uplink(&self, pcm: &[i16]) -> Vec<UplinkMessage> {
        vec![UplinkMessage::Binary(pcm_to_bytes(pcm))]
    }

    fn encode_dtmf(&self, digit: char) -> Option<UplinkMessage> {
        Some(UplinkMessage::Text(
            serde_json::json!({ "type": "dtmf", "digit": digit.to_string() }).to_string(),
        ))
    }

    fn cancel_response(&self) -> Option<UplinkMessage> {
        // Raw PCM has no server-side generation to cancel; barge-in is purely
        // a playout-side mute for this protocol.
        None
    }

    fn decode_text(&self, text: &str) -> Vec<DownlinkEvent> {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            return Vec::new();
        };
        match value.get("type").and_then(|v| v.as_str()) {
            Some("dtmf") => {
                let digit = value
                    .get("digit")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.chars().next());
                digit
                    .map(|d| DownlinkEvent::Dtmf { digit: d })
                    .into_iter()
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    fn decode_binary(&self, bytes: &[u8]) -> Vec<DownlinkEvent> {
        let pcm = bytes_to_pcm(bytes);
        if pcm.is_empty() {
            return Vec::new();
        }
        vec![DownlinkEvent::AudioDelta {
            pcm,
            // The bridge loop knows the negotiated WS rate; 0 defers to it.
            sample_rate: 0,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(rate: u32) -> RealtimeParams {
        RealtimeParams {
            url: "ws://127.0.0.1:9100?samplerate=8000".into(),
            protocol: RealtimeProtocolKind::PcmBridge,
            api_key: None,
            model: None,
            voice: None,
            instructions: None,
            sample_rate: Some(rate),
            timeout_ms: None,
            extra_headers: vec![("X-Custom".into(), "v".into())],
            hangup_on_disconnect: true,
        }
    }

    #[test]
    fn uplink_is_headerless_pcm16le() {
        let p = PcmBridge;
        let msgs = p.encode_uplink(&[1, -2, 3]);
        assert_eq!(msgs.len(), 1);
        match &msgs[0] {
            UplinkMessage::Binary(bytes) => {
                assert_eq!(bytes, &[1u8, 0, 0xFE, 0xFF, 3, 0]);
            }
            other => panic!("expected binary, got {other:?}"),
        }
    }

    #[test]
    fn downlink_binary_decodes_to_audio() {
        let p = PcmBridge;
        let events = p.decode_binary(&[7u8, 0, 0, 0]);
        assert_eq!(events.len(), 1);
        match &events[0] {
            DownlinkEvent::AudioDelta { pcm, sample_rate } => {
                assert_eq!(pcm, &[7, 0]);
                assert_eq!(*sample_rate, 0, "rate deferred to bridge loop");
            }
            other => panic!("expected audio delta, got {other:?}"),
        }
        assert!(p.decode_binary(&[]).is_empty());
        assert!(p.decode_binary(&[0xAB]).is_empty(), "odd byte truncated");
    }

    #[test]
    fn dtmf_roundtrips_as_json_text() {
        let p = PcmBridge;
        let msg = p.encode_dtmf('5').expect("dtmf supported");
        let UplinkMessage::Text(text) = &msg else {
            panic!("expected text");
        };
        let events = p.decode_text(text);
        assert_eq!(events, vec![DownlinkEvent::Dtmf { digit: '5' }]);
        // Non-DTMF text is ignored, not an error.
        assert!(p.decode_text("{\"type\":\"ping\"}").is_empty());
        assert!(p.decode_text("not json").is_empty());
    }

    #[test]
    fn headers_pass_through_and_rate_defaults() {
        let p = PcmBridge;
        // sample_rate: None → protocol default; Some(16000) wins.
        let mut no_rate = params(8000);
        no_rate.sample_rate = None;
        assert_eq!(p.sample_rate(&no_rate), 8000);
        assert_eq!(p.sample_rate(&params(16000)), 16000);
        let hdrs = p.upgrade_headers(&params(8000));
        assert_eq!(hdrs, vec![("X-Custom".to_string(), "v".to_string())]);
        assert!(p.session_open_messages(&params(8000)).is_empty());
        assert!(p.cancel_response().is_none());
        assert_eq!(p.connect_url(&params(8000)), params(8000).url);
    }
}
