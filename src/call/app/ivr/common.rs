use crate::call::app::controller::{CallController, DtmfCollectConfig};
use crate::call::app::{AppAction, ApplicationContext};
use crate::callrecord::CallRecordHangupReason;
use crate::http_util::{self, HttpFetchOptions};
use crate::tts::TtsService;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::config::{ActionNode, EntryAction};

pub enum ActionResult {
    Terminal(TerminalAction),
    ChainedTo(ActionNode),
    WaitFor(WaitEvent),
}

pub enum TerminalAction {
    Transfer(String),
    Hangup {
        reason: Option<CallRecordHangupReason>,
        code: Option<u16>,
    },
    Exit,
}

pub enum WaitEvent {
    AudioComplete {
        interrupted: bool,
    },
    DtmfCollected {
        digit: String,
    },
    ApiResponse {
        status: u16,
        body: serde_json::Value,
    },
    DtmfTimeout,
    RecordingComplete {
        url: String,
        duration_secs: u64,
    },
    InputVoice {
        text: String,
        confidence: f32,
    },
    /// Audio source was requested (e.g. via tts_text) but no TTS service is available.
    /// The caller should inform the provider and request a fallback action.
    NoAudio,
}

#[derive(Debug, Default, Clone)]
pub struct SessionData {
    pub variables: HashMap<String, String>,
    /// SIP headers from the original INVITE (excluding standard transport headers).
    pub sip_headers: HashMap<String, String>,
}

pub fn substitute_vars(s: &str, vars: &HashMap<String, String>) -> String {
    let mut result = s.to_string();
    for (key, value) in vars {
        let placeholder = format!("${}$", key);
        result = result.replace(&placeholder, value);
    }
    result
}

pub fn resolve_audio_path(
    file: Option<&str>,
    tts_text: Option<&str>,
    tts_voice: Option<&str>,
) -> Option<String> {
    if let Some(f) = file.filter(|f| !f.is_empty()) {
        return Some(f.to_string());
    }
    if let Some(text) = tts_text.filter(|t| !t.is_empty()) {
        let mut uri = format!("tts://{}", text);
        if let Some(voice) = tts_voice.filter(|v| !v.is_empty()) {
            uri.push_str(&format!("?voice={}", voice));
        }
        return Some(uri);
    }
    None
}

pub async fn resolve_audio(
    file: Option<&str>,
    tts_text: Option<&str>,
    tts_voice: Option<&str>,
    tts_service: Option<&Arc<TtsService>>,
) -> Option<String> {
    if let Some(f) = file.filter(|f| !f.is_empty()) {
        if let Some(rest) = f.strip_prefix("tts://") {
            let (encoded_text, voice) = if let Some((t, q)) = rest.split_once('?') {
                let v = q.strip_prefix("voice=").filter(|v| !v.is_empty());
                (t, v)
            } else {
                (rest, None)
            };
            let tts_text = urlencoding::decode(encoded_text)
                .unwrap_or(std::borrow::Cow::Borrowed(encoded_text));
            return synthesize_tts(&tts_text, voice, tts_service).await;
        }
        return Some(f.to_string());
    }
    if let Some(text) = tts_text.filter(|t| !t.is_empty()) {
        return synthesize_tts(text, tts_voice, tts_service).await;
    }
    None
}

async fn synthesize_tts(
    text: &str,
    voice: Option<&str>,
    tts_service: Option<&Arc<TtsService>>,
) -> Option<String> {
    if let Some(service) = tts_service {
        match service.synthesize(text, voice).await {
            Ok(path) => return Some(path),
            Err(e) => {
                tracing::warn!(text = %text, error = %e, "TTS synthesis failed");
            }
        }
    }
    // Fallback: try edge-cli if no TTS service configured
    tracing::warn!(text = %text, "TTS service not configured, falling back to edge-cli");
    let voice_str = voice.unwrap_or("zh-CN-XiaoxiaoNeural").to_string();
    let fallback_cfg = crate::tts::TtsConfig {
        cache_dir: std::env::temp_dir()
            .join("rustpbx_tts_cache")
            .to_string_lossy()
            .to_string(),
        cache_ttl_seconds: 86400,
        driver: crate::tts::TtsDriverConfig::Cli(crate::tts::CliTtsConfig {
            command: "edge-cli".to_string(),
            args: vec![
                "speak".to_string(),
                "--text".to_string(),
                "{text}".to_string(),
                "--voice".to_string(),
                "{voice}".to_string(),
                "--output".to_string(),
                "{output}".to_string(),
            ],
            output_format: "mp3".to_string(),
        }),
    };
    let fallback_service = crate::tts::TtsService::new(fallback_cfg);
    match fallback_service.synthesize(text, Some(&voice_str)).await {
        Ok(audio_path) => {
            tracing::info!(path = %audio_path, "edge-cli TTS synthesis succeeded");
            Some(audio_path)
        }
        Err(e) => {
            tracing::warn!(text = %text, error = %e, "edge-cli TTS fallback failed");
            None
        }
    }
}

async fn fetch_tts_text_from_api(
    url: &str,
    sess: &SessionData,
    ctx: &ApplicationContext,
) -> Option<String> {
    let url = substitute_vars(url, &sess.variables);
    let opts = HttpFetchOptions::new().with_timeout(Duration::from_secs(30));
    match http_util::fetch_json(&ctx.http_client, &url, &opts).await {
        Ok(body) => extract_tts_text(&body),
        Err(e) => {
            tracing::warn!(url = %url, error = %e, "fetch_tts_text_from_api failed");
            None
        }
    }
}

fn extract_tts_text(value: &serde_json::Value) -> Option<String> {
    if let Some(obj) = value.as_object() {
        for key in &["tts_text", "text", "message", "content", "speech"] {
            if let Some(s) = obj.get(*key).and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
        if let Some(data) = obj.get("data") {
            if let Some(s) = extract_tts_text(data) {
                return Some(s);
            }
        }
        if let Some(rd) = obj.get("response_data") {
            if let Some(s) = extract_tts_text(rd) {
                return Some(s);
            }
        }
    }
    if let Some(s) = value.as_str() {
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
pub async fn execute_action(
    action: &EntryAction,
    ctrl: &mut CallController,
    ctx: &ApplicationContext,
    sess: &mut SessionData,
    tts_service: Option<&Arc<TtsService>>,
) -> anyhow::Result<ActionResult> {
    match action {
        EntryAction::Transfer { target } => {
            let t = substitute_vars(target, &sess.variables);
            Ok(ActionResult::Terminal(TerminalAction::Transfer(t)))
        }
        EntryAction::Queue { target, .. } => {
            let t = substitute_vars(target, &sess.variables);
            Ok(ActionResult::Terminal(TerminalAction::Transfer(format!(
                "queue:{}",
                t
            ))))
        }
        EntryAction::Voicemail { target } => {
            let t = substitute_vars(target, &sess.variables);
            Ok(ActionResult::Terminal(TerminalAction::Transfer(format!(
                "voicemail:{}",
                t
            ))))
        }
        EntryAction::Hangup {
            prompt,
            prompt_text,
            prompt_voice,
            ..
        } => {
            if let Some(a) = resolve_audio(
                prompt.as_deref(),
                prompt_text.as_deref(),
                prompt_voice.as_deref(),
                tts_service,
            )
            .await
            {
                ctrl.play_audio(a, false).await?;
                return Ok(ActionResult::WaitFor(WaitEvent::AudioComplete {
                    interrupted: false,
                }));
            }
            Ok(ActionResult::Terminal(TerminalAction::Hangup {
                reason: None,
                code: None,
            }))
        }
        EntryAction::PlayAndHangup {
            prompt,
            prompt_text,
            prompt_voice,
            code,
        } => {
            let audio = resolve_audio(
                prompt.as_deref(),
                prompt_text.as_deref(),
                prompt_voice.as_deref(),
                tts_service,
            )
            .await;
            if let Some(a) = audio {
                ctrl.play_audio(a, false).await?;
                return Ok(ActionResult::WaitFor(WaitEvent::AudioComplete {
                    interrupted: false,
                }));
            }
            Ok(ActionResult::Terminal(TerminalAction::Hangup {
                reason: None,
                code: *code,
            }))
        }

        EntryAction::Prompt {
            file,
            tts_text,
            tts_voice,
            record_name_list,
            interruptible,
            tts_api_url,
            ..
        } => {
            let resolved_text = if tts_api_url.is_some() {
                match fetch_tts_text_from_api(tts_api_url.as_deref().unwrap(), sess, ctx).await {
                    Some(text) => Some(text),
                    None => tts_text.clone(),
                }
            } else {
                tts_text.clone()
            };
            let audio = if let Some(rnl) = record_name_list {
                Some(rnl.clone())
            } else {
                resolve_audio(
                    file.as_deref(),
                    resolved_text.as_deref(),
                    tts_voice.as_deref(),
                    tts_service,
                )
                .await
            };
            if let Some(a) = audio {
                ctrl.play_audio_with_options(a, Some("ivr_prompt".into()), false, *interruptible)
                    .await?;
                Ok(ActionResult::WaitFor(WaitEvent::AudioComplete {
                    interrupted: false,
                }))
            } else {
                Ok(ActionResult::WaitFor(WaitEvent::NoAudio))
            }
        }

        EntryAction::DtmfMenu {
            greeting,
            greeting_text,
            greeting_record_list,
            greeting_voice,
            greeting_api_url,
            ..
        } => {
            let resolved_greeting_text = if greeting_api_url.is_some() {
                match fetch_tts_text_from_api(greeting_api_url.as_deref().unwrap(), sess, ctx).await
                {
                    Some(text) => Some(text),
                    None => greeting_text.clone(),
                }
            } else {
                greeting_text.clone()
            };
            let audio = if let Some(grl) = greeting_record_list {
                Some(grl.clone())
            } else {
                resolve_audio(
                    greeting.as_deref(),
                    resolved_greeting_text.as_deref(),
                    greeting_voice.as_deref(),
                    tts_service,
                )
                .await
            };
            if let Some(a) = audio {
                ctrl.play_audio_with_options(a, Some("ivr_menu_greeting".into()), false, true)
                    .await?;
                Ok(ActionResult::WaitFor(WaitEvent::AudioComplete {
                    interrupted: false,
                }))
            } else {
                Ok(ActionResult::WaitFor(WaitEvent::NoAudio))
            }
        }

        EntryAction::CollectDtmf {
            min_digits,
            max_digits,
            timeout_ms,
            terminator,
            prompt,
        } => {
            if let Some(p) = prompt {
                if let Some(a) = resolve_audio(Some(p), None, None, tts_service).await {
                    ctrl.play_audio(a, false).await?;
                }
            }
            let config = DtmfCollectConfig {
                min_digits: *min_digits,
                max_digits: *max_digits,
                timeout: Duration::from_millis(*timeout_ms),
                terminator: terminator.as_ref().and_then(|t| t.chars().next()),
                play_prompt: None,
                inter_digit_timeout: None,
            };
            let digits = ctrl.collect_dtmf(config).await?;
            sess.variables.insert("dtmf_input".into(), digits.clone());
            if digits.is_empty() {
                Ok(ActionResult::WaitFor(WaitEvent::DtmfTimeout))
            } else {
                Ok(ActionResult::WaitFor(WaitEvent::DtmfCollected {
                    digit: digits,
                }))
            }
        }

        EntryAction::InputPhone {
            prompt,
            prompt_text,
            prompt_voice,
            min_digits,
            max_digits,
        } => {
            let audio = resolve_audio(
                prompt.as_deref(),
                prompt_text.as_deref(),
                prompt_voice.as_deref(),
                tts_service,
            )
            .await;
            if let Some(a) = audio {
                ctrl.play_audio(a, false).await?;
            }
            let config = DtmfCollectConfig {
                min_digits: *min_digits,
                max_digits: *max_digits,
                timeout: Duration::from_millis(10000),
                terminator: Some('#'),
                play_prompt: None,
                inter_digit_timeout: Some(Duration::from_millis(3000)),
            };
            let digits = ctrl.collect_dtmf(config).await?;
            sess.variables.insert("phone_number".into(), digits.clone());
            if digits.is_empty() {
                Ok(ActionResult::WaitFor(WaitEvent::DtmfTimeout))
            } else {
                Ok(ActionResult::WaitFor(WaitEvent::DtmfCollected {
                    digit: digits,
                }))
            }
        }

        EntryAction::Api {
            url,
            method,
            headers,
            timeout,
            ..
        } => {
            let url = substitute_vars(url, &sess.variables);
            let method = method.as_deref().unwrap_or("GET");
            let opts = HttpFetchOptions::new()
                .with_headers(headers.clone())
                .with_timeout(Duration::from_secs(*timeout));

            let req_builder = if method.eq_ignore_ascii_case("GET") {
                let params = [
                    (
                        "session_id",
                        sess.variables.get("session_id").map_or("", |v| v),
                    ),
                    ("caller", sess.variables.get("caller").map_or("", |v| v)),
                ];
                ctx.http_client.get(&url).query(&params)
            } else {
                let body = serde_json::json!({
                    "session_id": sess.variables.get("session_id"),
                    "caller": sess.variables.get("caller"),
                    "callee": sess.variables.get("callee"),
                    "variables": sess.variables,
                });
                ctx.http_client.post(&url).json(&body)
            };

            match crate::http_util::execute_request(req_builder, &opts.headers, opts.timeout).await
            {
                Ok(response) => {
                    let status = response.status().as_u16();
                    let body: serde_json::Value =
                        response.json().await.unwrap_or(serde_json::Value::Null);
                    sess.variables
                        .insert("api_status".into(), status.to_string());
                    if let Some(s) = body.as_str() {
                        sess.variables.insert("api_result".into(), s.to_string());
                    } else if !body.is_null() {
                        sess.variables.insert("api_result".into(), body.to_string());
                    }
                    Ok(ActionResult::WaitFor(WaitEvent::ApiResponse {
                        status,
                        body,
                    }))
                }
                Err(e) => {
                    let status = 0u16;
                    let body = serde_json::Value::Null;
                    sess.variables.insert("api_status".into(), e.to_string());
                    Ok(ActionResult::WaitFor(WaitEvent::ApiResponse {
                        status,
                        body,
                    }))
                }
            }
        }

        EntryAction::JumpIvr {
            route_point,
            params,
        } => {
            let rp = substitute_vars(route_point, &sess.variables);
            let target = format!("toivr:{}", rp);
            sess.variables.insert(
                "jump_params".into(),
                serde_json::to_string(params).unwrap_or_default(),
            );
            Ok(ActionResult::Terminal(TerminalAction::Transfer(target)))
        }

        EntryAction::RouteToAgent {
            target,
            skill_group_id,
            key_id,
            channel_code,
        } => {
            let t = substitute_vars(target, &sess.variables);
            let mut node_value = String::new();
            if let Some(sg) = skill_group_id {
                node_value = format!("from_gate:{}", sg);
            }
            if let Some(kid) = key_id {
                if !node_value.is_empty() {
                    node_value.push(':');
                }
                node_value.push_str(kid);
            }
            if let Some(cc) = channel_code {
                sess.variables.insert("channel_code".into(), cc.clone());
            }
            sess.variables.insert("route_node_value".into(), node_value);
            Ok(ActionResult::Terminal(TerminalAction::Transfer(t)))
        }

        EntryAction::VoipBridge {
            create_room_uri,
            headers,
            success,
            failure,
            ..
        } => {
            let uri = substitute_vars(create_room_uri, &sess.variables);
            sess.variables.insert("voip_room_uri".into(), uri.clone());
            for (k, v) in headers {
                sess.variables.insert(format!("voip_hdr_{}", k), v.clone());
            }
            if success.is_some() || failure.is_some() {
                sess.variables
                    .insert("voip_bridge_branch".into(), "true".into());
                Ok(ActionResult::Terminal(TerminalAction::Transfer(format!(
                    "voip_bridge:{}",
                    uri
                ))))
            } else {
                Ok(ActionResult::Terminal(TerminalAction::Transfer(format!(
                    "voip_bridge:{}",
                    uri
                ))))
            }
        }

        EntryAction::Torecord {
            prompt,
            beep,
            max_duration_secs,
        } => {
            if let Some(p) = prompt {
                if let Some(a) = resolve_audio(Some(p), None, None, tts_service).await {
                    ctrl.play_audio(a, false).await?;
                }
            }
            let recording_path = format!(
                "recordings/{}/{}.wav",
                sess.variables
                    .get("session_id")
                    .unwrap_or(&"unknown".into()),
                chrono::Utc::now().format("%Y%m%d_%H%M%S")
            );
            ctrl.start_recording(
                recording_path,
                max_duration_secs.map(|s| Duration::from_secs(s as u64)),
                *beep,
            )
            .await?;
            Ok(ActionResult::WaitFor(WaitEvent::RecordingComplete {
                url: String::new(),
                duration_secs: 0,
            }))
        }

        EntryAction::InputVoice { scene, .. } => {
            sess.variables.insert("asr_scene".into(), scene.clone());
            // ASR not supported yet — return immediately via WaitFor
            // so the provider can decide the next step
            Ok(ActionResult::WaitFor(WaitEvent::InputVoice {
                text: String::new(),
                confidence: 0.0,
            }))
        }

        EntryAction::Play { .. }
        | EntryAction::Menu { .. }
        | EntryAction::Repeat
        | EntryAction::Back
        | EntryAction::CollectExtension { .. }
        | EntryAction::Collect { .. }
        | EntryAction::Webhook { .. } => Err(anyhow::anyhow!(
            "{:?} should be handled by tree mode state machine, not step executor",
            std::mem::discriminant(action)
        )),
    }
}

impl From<TerminalAction> for AppAction {
    fn from(t: TerminalAction) -> Self {
        match t {
            TerminalAction::Transfer(target) => AppAction::Transfer(target),
            TerminalAction::Hangup { reason, code } => AppAction::Hangup { reason, code },
            TerminalAction::Exit => AppAction::Exit,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_substitute_vars() {
        let mut vars = HashMap::new();
        vars.insert("user_phone".into(), "1001".into());
        vars.insert("input".into(), "123".into());
        let url = "http://api/check?phone=$user_phone$&input=$input$";
        let result = substitute_vars(url, &vars);
        assert_eq!(result, "http://api/check?phone=1001&input=123");
    }

    #[test]
    fn test_substitute_vars_missing_keeps_placeholder() {
        let vars = HashMap::new();
        let result = substitute_vars("url=$missing$", &vars);
        assert_eq!(result, "url=$missing$");
    }

    #[test]
    fn test_resolve_audio_path_file() {
        let result = resolve_audio_path(Some("welcome.wav"), None, None);
        assert_eq!(result, Some("welcome.wav".into()));
    }

    #[test]
    fn test_resolve_audio_path_tts() {
        let result = resolve_audio_path(None, Some("你好世界"), Some("zh-CN-XiaoxiaoNeural"));
        assert!(result.unwrap().starts_with("tts://"));
    }

    #[test]
    fn test_substitute_vars_empty_vars() {
        let vars = HashMap::new();
        let result = substitute_vars("hello", &vars);
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_substitute_vars_multiple() {
        let mut vars = HashMap::new();
        vars.insert("a".into(), "1".into());
        vars.insert("b".into(), "2".into());
        let result = substitute_vars("$a$- $b$", &vars);
        assert_eq!(result, "1- 2");
    }
}
