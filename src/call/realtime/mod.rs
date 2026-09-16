//! Realtime voice bridge — bidirectional streaming audio between a call leg
//! and an external realtime (AI voice) endpoint over WebSocket.
//!
//! Two protocol adapters are provided behind the [`RealtimeProtocol`] trait:
//!
//! - [`pcm::PcmBridge`] — raw headerless PCM16-LE binary frames both ways,
//!   DTMF as `{"type":"dtmf","digit":"5"}` JSON text. Speaks to self-hosted
//!   realtime model servers (same wire format as the `voip_bridge:` transfer
//!   target).
//! - [`openai::OpenAiRealtime`] — OpenAI Realtime API JSON event framing
//!   (base64 PCM16 inside `input_audio_buffer.append` /
//!   `response.audio.delta`), server VAD speech events for barge-in,
//!   transcription deltas and function-call passthrough.
//!
//! The PBX is a **transport**: transcripts and function calls are surfaced
//! as RWI [`crate::rwi::RealtimeEvent`]s for the business side to act on —
//! the core never executes tools or applies business logic.
//!
//! ## Authentication
//!
//! API keys travel in the WebSocket **upgrade request headers**
//! (`Authorization: Bearer`/`Token`), never in the URL — URLs end up in
//! logs, sipflow captures and CDR metadata. Keys live in the `[[realtime]]`
//! config presets (or the `OPENAI_API_KEY` / `REALTIME_API_KEY` env
//! fallback); dialplans and app params reference a preset by name only.

pub mod bridge;
pub mod openai;
pub mod pcm;

use serde::{Deserialize, Serialize};

/// Which wire protocol a realtime endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RealtimeProtocolKind {
    /// Raw PCM16-LE binary + DTMF JSON (self-hosted / voip_bridge wire format).
    PcmBridge,
    /// OpenAI Realtime API JSON events.
    OpenAi,
}

impl RealtimeProtocolKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "pcm" | "pcm_bridge" | "pcm-bridge" => Some(Self::PcmBridge),
            "openai" | "openai-realtime" | "openai_realtime" => Some(Self::OpenAi),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PcmBridge => "pcm_bridge",
            Self::OpenAi => "openai",
        }
    }
}

/// Fully resolved connection parameters for one realtime session — the
/// result of merging app params with a `[[realtime]]` config preset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealtimeParams {
    /// WebSocket endpoint (`ws://` / `wss://`). For OpenAI this is the base
    /// realtime URL; the protocol adapter appends `?model=` when configured.
    pub url: String,
    pub protocol: RealtimeProtocolKind,
    /// Bearer/Token credential — upgrade-header only, never logged.
    pub api_key: Option<String>,
    /// Provider model id (OpenAI: `?model=` query + session configuration).
    pub model: Option<String>,
    /// Provider voice id (OpenAI `session.update`).
    pub voice: Option<String>,
    /// System prompt (OpenAI `session.update` instructions).
    pub instructions: Option<String>,
    /// PCM sample rate on the WebSocket side (Hz). Protocol defaults apply
    /// when unset (openai: 24000, pcm_bridge: 8000).
    pub sample_rate: Option<u32>,
    /// WebSocket connect timeout in milliseconds.
    pub timeout_ms: Option<u64>,
    /// Extra static upgrade headers (e.g. provider-specific auth schemes).
    #[serde(default)]
    pub extra_headers: Vec<(String, String)>,
    /// Hang the call up when the realtime WebSocket closes (default true).
    /// When false the call continues after a `realtime_disconnected` event.
    #[serde(default = "default_hangup_on_disconnect")]
    pub hangup_on_disconnect: bool,
}

fn default_hangup_on_disconnect() -> bool {
    true
}

impl RealtimeParams {
    /// Merge app params (`preset: "name"` plus overrides) with the
    /// configured `[[realtime]]` presets. Preset values win only where the
    /// app params do not explicitly override them; `api_key` can never be
    /// set from app params (config / env only — security boundary).
    pub fn resolve(
        value: &serde_json::Value,
        presets: Option<&[crate::config::RealtimePreset]>,
    ) -> anyhow::Result<Self> {
        let obj = value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("realtime app params must be a JSON object"))?;

        // `preset` picks a configured endpoint; inline `url` is for tests and
        // self-hosted endpoints.
        let mut preset_applied = false;
        let mut params = if let Some(name) = obj.get("preset").and_then(|v| v.as_str()) {
            let preset = presets
                .and_then(|list| list.iter().find(|p| p.name == name))
                .ok_or_else(|| {
                    anyhow::anyhow!("realtime preset '{name}' not found in [[realtime]] config")
                })?;
            preset_applied = true;
            Self {
                url: preset.url.clone(),
                protocol: preset
                    .protocol
                    .as_deref()
                    .and_then(RealtimeProtocolKind::parse)
                    .unwrap_or(RealtimeProtocolKind::OpenAi),
                api_key: preset.api_key.clone(),
                model: preset.model.clone(),
                voice: preset.voice.clone(),
                instructions: preset.instructions.clone(),
                sample_rate: preset.sample_rate,
                timeout_ms: preset.timeout_ms,
                extra_headers: Vec::new(),
                hangup_on_disconnect: preset.hangup_on_disconnect.unwrap_or(true),
            }
        } else {
            Self {
                url: String::new(),
                protocol: RealtimeProtocolKind::OpenAi,
                api_key: None,
                model: None,
                voice: None,
                instructions: None,
                sample_rate: None,
                timeout_ms: None,
                extra_headers: Vec::new(),
                hangup_on_disconnect: true,
            }
        };

        // Explicit app-param overrides.
        if let Some(url) = obj.get("url").and_then(|v| v.as_str()) {
            params.url = url.to_string();
        }
        if let Some(p) = obj.get("protocol").and_then(|v| v.as_str()) {
            params.protocol = RealtimeProtocolKind::parse(p)
                .ok_or_else(|| anyhow::anyhow!("unknown realtime protocol '{p}'"))?;
        }
        if let Some(m) = obj.get("model").and_then(|v| v.as_str()) {
            params.model = Some(m.to_string());
        }
        if let Some(v) = obj.get("voice").and_then(|v| v.as_str()) {
            params.voice = Some(v.to_string());
        }
        if let Some(i) = obj.get("instructions").and_then(|v| v.as_str()) {
            params.instructions = Some(i.to_string());
        }
        if let Some(r) = obj.get("sample_rate").and_then(|v| v.as_u64()) {
            params.sample_rate = Some(u32::try_from(r).unwrap_or(8000));
        }
        if let Some(t) = obj.get("timeout_ms").and_then(|v| v.as_u64()) {
            params.timeout_ms = Some(t);
        }
        if let Some(h) = obj.get("hangup_on_disconnect").and_then(|v| v.as_bool()) {
            params.hangup_on_disconnect = h;
        }

        if params.url.is_empty() {
            anyhow::bail!("realtime app params require `preset` or a `url`");
        }
        if !preset_applied && params.api_key.is_none() {
            // Inline endpoints may still carry the key via env fallback at
            // connect time (openai adapter) — nothing to enforce here.
        }
        let _ = preset_applied;
        Ok(params)
    }

    /// Env-var fallback chain for the API key, mirroring the Deepgram ASR
    /// precedent (`config wins, env fills in`).
    pub fn effective_api_key(&self) -> Option<String> {
        if let Some(key) = self.api_key.as_deref().filter(|k| !k.is_empty()) {
            return Some(key.to_string());
        }
        if self.protocol == RealtimeProtocolKind::OpenAi {
            if let Ok(key) = std::env::var("OPENAI_API_KEY") {
                if !key.is_empty() {
                    return Some(key);
                }
            }
        }
        std::env::var("REALTIME_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())
    }
}

/// A message to write to the realtime WebSocket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UplinkMessage {
    Text(String),
    Binary(Vec<u8>),
}

/// Decoded downlink event from the realtime endpoint.
#[derive(Debug, Clone, PartialEq)]
pub enum DownlinkEvent {
    /// AI audio to play to the caller (PCM16 mono at `sample_rate`).
    AudioDelta { pcm: Vec<i16>, sample_rate: u32 },
    /// Server VAD: the caller started speaking — barge-in (mute playout).
    SpeechStarted,
    /// Server VAD: the caller stopped speaking — resume playout.
    SpeechStopped,
    /// Partial transcript of the AI's speech.
    TranscriptDelta { text: String },
    /// Final transcript of the caller's speech.
    TranscriptFinal { text: String },
    /// Tool/function call requested by the model — passthrough only.
    FunctionCall {
        name: String,
        arguments: String,
        call_id: Option<String>,
    },
    /// Caller-side DTMF digit relayed from the endpoint.
    Dtmf { digit: char },
    /// Session handshake completed (session.created / first ready signal).
    SessionReady,
    /// Endpoint reported an error.
    Error { message: String },
}

/// Wire-protocol adapter for one realtime endpoint flavor.
pub trait RealtimeProtocol: Send + Sync {
    fn kind(&self) -> RealtimeProtocolKind;

    /// PCM sample rate used on the WebSocket side.
    fn sample_rate(&self, params: &RealtimeParams) -> u32;

    /// Final connect URL (adapters may append query params, e.g. `?model=`).
    fn connect_url(&self, params: &RealtimeParams) -> String;

    /// Extra WebSocket upgrade headers (auth etc.). The Authorization header
    /// must be built here when the protocol uses one — never in the URL.
    fn upgrade_headers(&self, params: &RealtimeParams) -> Vec<(String, String)>;

    /// Messages to send immediately after the socket opens (session
    /// configuration handshake). Empty for the raw PCM bridge.
    fn session_open_messages(&self, params: &RealtimeParams) -> Vec<UplinkMessage>;

    /// Encode one uplink PCM frame (mono at [`Self::sample_rate`]).
    fn encode_uplink(&self, pcm: &[i16]) -> Vec<UplinkMessage>;

    /// Encode a caller DTMF digit for the endpoint (None = unsupported).
    fn encode_dtmf(&self, digit: char) -> Option<UplinkMessage>;

    /// Message to send when barge-in mutes playout (None = unsupported).
    fn cancel_response(&self) -> Option<UplinkMessage>;

    /// Decode a TEXT frame from the endpoint.
    fn decode_text(&self, text: &str) -> Vec<DownlinkEvent>;

    /// Decode a BINARY frame from the endpoint.
    fn decode_binary(&self, bytes: &[u8]) -> Vec<DownlinkEvent>;
}

/// Build the protocol adapter for a params set.
pub fn protocol_for(params: &RealtimeParams) -> Box<dyn RealtimeProtocol> {
    match params.protocol {
        RealtimeProtocolKind::PcmBridge => Box::new(pcm::PcmBridge),
        RealtimeProtocolKind::OpenAi => Box::new(openai::OpenAiRealtime),
    }
}

/// PCM16 mono → little-endian bytes.
pub fn pcm_to_bytes(pcm: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pcm.len() * 2);
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Little-endian bytes → PCM16 mono (truncates a trailing odd byte).
pub fn bytes_to_pcm(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_kind_parses_aliases() {
        assert_eq!(
            RealtimeProtocolKind::parse("openai"),
            Some(RealtimeProtocolKind::OpenAi)
        );
        assert_eq!(
            RealtimeProtocolKind::parse("openai-realtime"),
            Some(RealtimeProtocolKind::OpenAi)
        );
        assert_eq!(
            RealtimeProtocolKind::parse("pcm"),
            Some(RealtimeProtocolKind::PcmBridge)
        );
        assert_eq!(
            RealtimeProtocolKind::parse("pcm-bridge"),
            Some(RealtimeProtocolKind::PcmBridge)
        );
        assert_eq!(RealtimeProtocolKind::parse("grpc"), None);
    }

    #[test]
    fn pcm_roundtrip_le() {
        let pcm = vec![-1i16, 32767, -32768, 42];
        let bytes = pcm_to_bytes(&pcm);
        assert_eq!(bytes.len(), 8);
        assert_eq!(bytes_to_pcm(&bytes), pcm);
        // Odd trailing byte is truncated, not misinterpreted.
        let mut odd = bytes.clone();
        odd.push(0xAB);
        assert_eq!(bytes_to_pcm(&odd), pcm);
    }

    #[test]
    fn resolve_requires_preset_or_url() {
        let err = RealtimeParams::resolve(&serde_json::json!({}), None).unwrap_err();
        assert!(err.to_string().contains("preset"), "got: {err}");

        let ok = RealtimeParams::resolve(
            &serde_json::json!({"url": "ws://127.0.0.1:9000", "protocol": "pcm"}),
            None,
        )
        .expect("inline url resolves");
        assert_eq!(ok.protocol, RealtimeProtocolKind::PcmBridge);
        assert!(ok.hangup_on_disconnect);
    }

    #[test]
    fn resolve_preset_merges_with_overrides() {
        let presets = vec![crate::config::RealtimePreset {
            name: "support-bot".into(),
            url: "wss://api.openai.com/v1/realtime".into(),
            protocol: Some("openai".into()),
            api_key: Some("sk-secret".into()),
            model: Some("gpt-realtime".into()),
            voice: Some("alloy".into()),
            instructions: Some("You are a support agent".into()),
            sample_rate: None,
            timeout_ms: Some(5000),
            hangup_on_disconnect: Some(false),
        }];
        let params = RealtimeParams::resolve(
            &serde_json::json!({
                "preset": "support-bot",
                "voice": "verse",
                "hangup_on_disconnect": true,
            }),
            Some(&presets),
        )
        .expect("preset resolves");

        // Preset values fill; explicit params override.
        assert_eq!(params.url, "wss://api.openai.com/v1/realtime");
        assert_eq!(params.api_key.as_deref(), Some("sk-secret"));
        assert_eq!(params.model.as_deref(), Some("gpt-realtime"));
        assert_eq!(params.voice.as_deref(), Some("verse"));
        assert!(params.hangup_on_disconnect, "explicit override wins");
        assert_eq!(params.timeout_ms, Some(5000));
    }

    #[test]
    fn resolve_unknown_preset_fails() {
        let err =
            RealtimeParams::resolve(&serde_json::json!({"preset": "nope"}), Some(&[])).unwrap_err();
        assert!(err.to_string().contains("not found"), "got: {err}");
    }

    #[test]
    fn api_key_never_settable_from_app_params() {
        // The security boundary: `api_key` in app params must be ignored.
        let presets = vec![crate::config::RealtimePreset {
            name: "p".into(),
            url: "wss://x".into(),
            protocol: Some("openai".into()),
            api_key: Some("sk-from-config".into()),
            model: None,
            voice: None,
            instructions: None,
            sample_rate: None,
            timeout_ms: None,
            hangup_on_disconnect: None,
        }];
        let params = RealtimeParams::resolve(
            &serde_json::json!({"preset": "p", "api_key": "sk-evil"}),
            Some(&presets),
        )
        .expect("resolves");
        assert_eq!(params.api_key.as_deref(), Some("sk-from-config"));
    }
}
