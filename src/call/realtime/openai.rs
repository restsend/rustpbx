//! OpenAI Realtime API protocol adapter.
//!
//! Wire format (wss, JSON text events; audio rides as base64 PCM16):
//! - Uplink audio: `{"type":"input_audio_buffer.append","audio":"<b64>"}` —
//!   mono PCM16 at 24 kHz (16 kHz also accepted; we upsample the leg).
//! - Downlink audio: `{"type":"response.audio.delta","delta":"<b64>"}`.
//! - Barge-in: server VAD emits `input_audio_buffer.speech_started` /
//!   `speech_stopped`; we mute playout and send `response.cancel`.
//! - Transcripts: `response.audio_transcript.delta` (AI speech) and
//!   `conversation.item.input_audio_transcription.completed` (caller speech).
//! - Tools: `response.output_item.done` with `item.type == "function_call"`
//!   is surfaced as a passthrough [`DownlinkEvent::FunctionCall`].
//!
//! Authentication travels in the upgrade request headers
//! (`Authorization: Bearer <key>` + `OpenAI-Beta: realtime=v1`) — never in
//! the URL, which ends up in logs and CDRs.

use super::{
    pcm_to_bytes, DownlinkEvent, RealtimeParams, RealtimeProtocol, RealtimeProtocolKind,
    UplinkMessage,
};
use base64::Engine as _;

/// PCM16 mono rate OpenAI expects by default.
pub const OPENAI_SAMPLE_RATE: u32 = 24000;

pub struct OpenAiRealtime;

impl RealtimeProtocol for OpenAiRealtime {
    fn kind(&self) -> RealtimeProtocolKind {
        RealtimeProtocolKind::OpenAi
    }

    fn sample_rate(&self, _params: &RealtimeParams) -> u32 {
        OPENAI_SAMPLE_RATE
    }

    fn connect_url(&self, params: &RealtimeParams) -> String {
        let mut url = params.url.clone();
        if let Some(model) = params.model.as_deref().filter(|m| !m.is_empty()) {
            let sep = if url.contains('?') { '&' } else { '?' };
            url.push(sep);
            url.push_str("model=");
            url.push_str(model);
        }
        url
    }

    fn upgrade_headers(&self, params: &RealtimeParams) -> Vec<(String, String)> {
        let mut headers = Vec::with_capacity(params.extra_headers.len() + 2);
        if let Some(key) = params.api_key.as_deref().filter(|k| !k.is_empty()) {
            headers.push(("Authorization".to_string(), format!("Bearer {key}")));
        }
        headers.push(("OpenAI-Beta".to_string(), "realtime=v1".to_string()));
        headers.extend(params.extra_headers.iter().cloned());
        headers
    }

    fn session_open_messages(&self, params: &RealtimeParams) -> Vec<UplinkMessage> {
        let mut session = serde_json::json!({
            "type": "session.update",
            "session": {
                "modalities": ["text", "audio"],
                "input_audio_format": "pcm16",
                "output_audio_format": "pcm16",
                "turn_detection": {
                    "type": "server_vad",
                },
            },
        });
        if let Some(voice) = params.voice.as_deref().filter(|v| !v.is_empty()) {
            session["session"]["voice"] = serde_json::json!(voice);
        }
        if let Some(instructions) = params.instructions.as_deref().filter(|i| !i.is_empty()) {
            session["session"]["instructions"] = serde_json::json!(instructions);
        }
        vec![UplinkMessage::Text(session.to_string())]
    }

    fn encode_uplink(&self, pcm: &[i16]) -> Vec<UplinkMessage> {
        if pcm.is_empty() {
            return Vec::new();
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(pcm_to_bytes(pcm));
        vec![UplinkMessage::Text(
            serde_json::json!({
                "type": "input_audio_buffer.append",
                "audio": encoded,
            })
            .to_string(),
        )]
    }

    fn encode_dtmf(&self, digit: char) -> Option<UplinkMessage> {
        // Realtime sessions have no DTMF concept; surface digits as a short
        // conversation item so prompt-driven flows can react to menus.
        Some(UplinkMessage::Text(
            serde_json::json!({
                "type": "response.create",
                "response": {
                    "instructions": format!("The caller pressed the DTMF digit {digit}."),
                },
            })
            .to_string(),
        ))
    }

    fn cancel_response(&self) -> Option<UplinkMessage> {
        Some(UplinkMessage::Text(
            serde_json::json!({ "type": "response.cancel" }).to_string(),
        ))
    }

    fn decode_text(&self, text: &str) -> Vec<DownlinkEvent> {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            return Vec::new();
        };
        let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match event_type {
            "response.audio.delta" => {
                let Some(delta) = value.get("delta").and_then(|v| v.as_str()) else {
                    return Vec::new();
                };
                match base64::engine::general_purpose::STANDARD.decode(delta) {
                    Ok(bytes) => vec![DownlinkEvent::AudioDelta {
                        pcm: super::bytes_to_pcm(&bytes),
                        sample_rate: OPENAI_SAMPLE_RATE,
                    }],
                    Err(_) => Vec::new(),
                }
            }
            "input_audio_buffer.speech_started" => vec![DownlinkEvent::SpeechStarted],
            "input_audio_buffer.speech_stopped" => vec![DownlinkEvent::SpeechStopped],
            "response.audio_transcript.delta" => string_event(&value, "delta", |text| {
                DownlinkEvent::TranscriptDelta { text }
            }),
            "conversation.item.input_audio_transcription.completed" => {
                string_event(&value, "transcript", |text| {
                    DownlinkEvent::TranscriptFinal { text }
                })
            }
            "session.created" => vec![DownlinkEvent::SessionReady],
            "error" => {
                let message = value
                    .pointer("/error/message")
                    .and_then(|v| v.as_str())
                    .or_else(|| value.get("message").and_then(|v| v.as_str()))
                    .unwrap_or("unknown realtime error")
                    .to_string();
                vec![DownlinkEvent::Error { message }]
            }
            "response.output_item.done" => {
                let item = value.get("item");
                let is_fn = item
                    .and_then(|i| i.get("type"))
                    .and_then(|v| v.as_str())
                    .is_some_and(|t| t == "function_call");
                if !is_fn {
                    return Vec::new();
                }
                let name = item
                    .and_then(|i| i.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let arguments = item
                    .and_then(|i| i.get("arguments"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let call_id = item
                    .and_then(|i| i.get("call_id"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                vec![DownlinkEvent::FunctionCall {
                    name,
                    arguments,
                    call_id,
                }]
            }
            _ => Vec::new(),
        }
    }

    fn decode_binary(&self, _bytes: &[u8]) -> Vec<DownlinkEvent> {
        // OpenAI realtime carries audio inside JSON events; binary frames
        // have no meaning on this protocol.
        Vec::new()
    }
}

fn string_event<F>(value: &serde_json::Value, field: &str, make: F) -> Vec<DownlinkEvent>
where
    F: FnOnce(String) -> DownlinkEvent,
{
    let Some(text) = value.get(field).and_then(|v| v.as_str()) else {
        return Vec::new();
    };
    if text.is_empty() {
        return Vec::new();
    }
    vec![make(text.to_string())]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> RealtimeParams {
        RealtimeParams {
            url: "wss://api.openai.com/v1/realtime".into(),
            protocol: RealtimeProtocolKind::OpenAi,
            api_key: Some("sk-test".into()),
            model: Some("gpt-4o-realtime-preview".into()),
            voice: Some("alloy".into()),
            instructions: Some("Be brief".into()),
            sample_rate: None,
            timeout_ms: None,
            extra_headers: Vec::new(),
            hangup_on_disconnect: true,
        }
    }

    #[test]
    fn url_appends_model_query() {
        let p = OpenAiRealtime;
        assert_eq!(
            p.connect_url(&params()),
            "wss://api.openai.com/v1/realtime?model=gpt-4o-realtime-preview"
        );
        let mut no_model = params();
        no_model.model = None;
        assert_eq!(p.connect_url(&no_model), no_model.url);
        // Existing query strings get '&' not '?'.
        let mut with_query = params();
        with_query.url.push_str("?beta=on");
        assert!(p.connect_url(&with_query).contains("?beta=on&model="));
    }

    #[test]
    fn auth_is_header_bearer_never_url() {
        let p = OpenAiRealtime;
        let url = p.connect_url(&params());
        assert!(!url.contains("sk-test"), "key must not leak into the url");
        let headers = p.upgrade_headers(&params());
        assert!(headers.contains(&("Authorization".to_string(), "Bearer sk-test".to_string())));
        assert!(headers.contains(&("OpenAI-Beta".to_string(), "realtime=v1".to_string())));
        // No key configured → no Authorization header, beta header still set.
        let mut anonymous = params();
        anonymous.api_key = None;
        let headers = p.upgrade_headers(&anonymous);
        assert!(!headers.iter().any(|(k, _)| k == "Authorization"));
    }

    #[test]
    fn session_update_carries_voice_and_instructions() {
        let p = OpenAiRealtime;
        let msgs = p.session_open_messages(&params());
        assert_eq!(msgs.len(), 1);
        let UplinkMessage::Text(text) = &msgs[0] else {
            panic!("expected text");
        };
        let v: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(v["type"], "session.update");
        assert_eq!(v["session"]["voice"], "alloy");
        assert_eq!(v["session"]["instructions"], "Be brief");
        assert_eq!(v["session"]["input_audio_format"], "pcm16");
        assert_eq!(v["session"]["turn_detection"]["type"], "server_vad");
    }

    #[test]
    fn uplink_audio_is_base64_append() {
        let p = OpenAiRealtime;
        let msgs = p.encode_uplink(&[1, -2, 3]);
        assert_eq!(msgs.len(), 1);
        let UplinkMessage::Text(text) = &msgs[0] else {
            panic!("expected text");
        };
        let v: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(v["type"], "input_audio_buffer.append");
        let b64 = v["audio"].as_str().expect("base64 audio");
        let decoded = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        assert_eq!(decoded, pcm_to_bytes(&[1, -2, 3]));
        assert!(p.encode_uplink(&[]).is_empty());
    }

    #[test]
    fn downlink_event_matrix() {
        let p = OpenAiRealtime;

        // Audio delta — base64 PCM16 → AudioDelta at 24k.
        let b64 = base64::engine::general_purpose::STANDARD.encode(pcm_to_bytes(&[100, -100]));
        let events = p.decode_text(&format!(
            r#"{{"type":"response.audio.delta","delta":"{b64}"}}"#
        ));
        assert_eq!(
            events,
            vec![DownlinkEvent::AudioDelta {
                pcm: vec![100, -100],
                sample_rate: OPENAI_SAMPLE_RATE,
            }]
        );

        // VAD speech events → barge-in signals.
        assert_eq!(
            p.decode_text(r#"{"type":"input_audio_buffer.speech_started"}"#),
            vec![DownlinkEvent::SpeechStarted]
        );
        assert_eq!(
            p.decode_text(r#"{"type":"input_audio_buffer.speech_stopped"}"#),
            vec![DownlinkEvent::SpeechStopped]
        );

        // AI transcript delta.
        assert_eq!(
            p.decode_text(r#"{"type":"response.audio_transcript.delta","delta":"Hello"}"#),
            vec![DownlinkEvent::TranscriptDelta {
                text: "Hello".into()
            }]
        );
        // Empty deltas are dropped.
        assert!(p
            .decode_text(r#"{"type":"response.audio_transcript.delta","delta":""}"#)
            .is_empty());

        // Caller transcript final.
        assert_eq!(
            p.decode_text(
                r#"{"type":"conversation.item.input_audio_transcription.completed","transcript":"hi there"}"#
            ),
            vec![DownlinkEvent::TranscriptFinal {
                text: "hi there".into()
            }]
        );

        // Session handshake.
        assert_eq!(
            p.decode_text(r#"{"type":"session.created"}"#),
            vec![DownlinkEvent::SessionReady]
        );

        // Error surfaces the provider message.
        assert_eq!(
            p.decode_text(r#"{"type":"error","error":{"message":"bad key"}}"#),
            vec![DownlinkEvent::Error {
                message: "bad key".into()
            }]
        );

        // Function call passthrough.
        let events = p.decode_text(
            r#"{"type":"response.output_item.done","item":{"type":"function_call","name":"lookup_order","arguments":"{\"id\":42}","call_id":"call_1"}}"#,
        );
        assert_eq!(
            events,
            vec![DownlinkEvent::FunctionCall {
                name: "lookup_order".into(),
                arguments: "{\"id\":42}".into(),
                call_id: Some("call_1".into()),
            }]
        );

        // Non-function output items are ignored; unknown types ignored; junk
        // text ignored (never an error).
        assert!(p
            .decode_text(r#"{"type":"response.output_item.done","item":{"type":"message"}}"#)
            .is_empty());
        assert!(p.decode_text(r#"{"type":"rate_limits.updated"}"#).is_empty());
        assert!(p.decode_text("garbage").is_empty());
        assert!(p.decode_binary(&[1, 2, 3]).is_empty());
    }

    #[test]
    fn barge_in_sends_response_cancel() {
        let p = OpenAiRealtime;
        let msg = p.cancel_response().expect("openai supports cancel");
        let UplinkMessage::Text(text) = msg else {
            panic!("expected text");
        };
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["type"], "response.cancel");
    }
}
