use crate::call::app::{
    AppAction, ApplicationContext, CallApp, CallAppType, CallController, RecordingInfo,
};
use crate::callrecord::CallRecordHangupReason;
use async_trait::async_trait;
use std::time::Duration;
use tracing::{info, warn};

/// Voicemail application that records a message for a specific extension.
pub struct VoicemailApp {
    extension: String,
    greeting_path: String,
    state: VoicemailState,
    recording_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VoicemailState {
    Greeting,
    Recording,
    Done,
}

impl VoicemailApp {
    pub fn new(extension: impl Into<String>) -> Self {
        Self {
            extension: extension.into(),
            greeting_path: DEFAULT_VOICEMAIL_GREETING.to_string(),
            state: VoicemailState::Greeting,
            recording_path: None,
        }
    }

    pub fn with_greeting_path(mut self, path: impl Into<String>) -> Self {
        self.greeting_path = path.into();
        self
    }
}

/// Built-in greeting played when neither the dialplan nor the PBX config
/// provides one.
pub const DEFAULT_VOICEMAIL_GREETING: &str = "sounds/voicemail/greeting.wav";

/// Resolve the voicemail greeting.
///
/// Priority: dialplan app param `greeting_path` > PBX-wide
/// `[proxy] voicemail_greeting` > built-in default. Empty/blank values fall
/// through to the next layer.
pub fn resolve_greeting_path(param: Option<&str>, global: Option<&str>) -> String {
    param
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| global.map(str::trim).filter(|s| !s.is_empty()))
        .unwrap_or(DEFAULT_VOICEMAIL_GREETING)
        .to_string()
}

#[async_trait]
impl CallApp for VoicemailApp {
    fn app_type(&self) -> CallAppType {
        CallAppType::Voicemail
    }

    fn name(&self) -> &str {
        "voicemail"
    }

    async fn on_enter(
        &mut self,
        ctrl: &mut CallController,
        _ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        info!(extension = %self.extension, "Voicemail app entered");
        ctrl.answer().await?;
        ctrl.record_trace(
            crate::call_errors::TraceEvent::new(
                crate::call_errors::TraceKind::Voicemail,
                format!(
                    "Voicemail: playing greeting for mailbox '{}'",
                    self.extension
                ),
            )
            .severity(crate::call_errors::ErrSeverity::Info),
        );
        ctrl.play_audio(&self.greeting_path, false).await?;
        Ok(AppAction::Continue)
    }

    async fn on_audio_complete(
        &mut self,
        _track_id: String,
        ctrl: &mut CallController,
        ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        if self.state == VoicemailState::Greeting {
            self.state = VoicemailState::Recording;
            let path = format!(
                "/tmp/voicemail_{}_{}.wav",
                self.extension, ctx.call_info.session_id
            );
            info!(
                path = %path,
                session_id = %ctx.call_info.session_id,
                "Starting voicemail recording"
            );
            ctrl.start_recording_mono(&path, Some(Duration::from_secs(300)), true)
                .await?;
            self.recording_path = Some(path);
        }
        Ok(AppAction::Continue)
    }

    async fn on_dtmf(
        &mut self,
        digit: String,
        ctrl: &mut CallController,
        ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        if digit == "#" && self.state == VoicemailState::Recording {
            info!("DTMF # received, stopping voicemail recording");
            // `stop_recording` consumes the RecordingComplete event, so invoke
            // on_record_complete directly to finalize (state → Done → hangup).
            if let Ok(info) = ctrl.stop_recording().await {
                return self.on_record_complete(info, ctrl, ctx).await;
            }
            self.state = VoicemailState::Done;
            return Ok(AppAction::Hangup {
                reason: Some(CallRecordHangupReason::BySystem),
                code: None,
            });
        }
        Ok(AppAction::Continue)
    }

    async fn on_record_complete(
        &mut self,
        info: RecordingInfo,
        ctrl: &mut CallController,
        _ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        info!(
            path = %info.path,
            duration = ?info.duration,
            size = info.size_bytes,
            "Voicemail recording completed"
        );
        ctrl.record_trace(
            crate::call_errors::TraceEvent::new(
                crate::call_errors::TraceKind::Voicemail,
                format!(
                    "Voicemail: message recorded ({}s) for mailbox '{}'",
                    info.duration.as_secs(),
                    self.extension
                ),
            )
            .severity(crate::call_errors::ErrSeverity::Info),
        );
        if self.state != VoicemailState::Done {
            self.state = VoicemailState::Done;
            return Ok(AppAction::Hangup {
                reason: Some(CallRecordHangupReason::BySystem),
                code: None,
            });
        }
        Ok(AppAction::Continue)
    }

    async fn on_timeout(
        &mut self,
        _timeout_id: String,
        ctrl: &mut CallController,
        _ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        if self.state == VoicemailState::Recording {
            warn!("Voicemail recording timed out, stopping");
            ctrl.stop_recording().await.ok();
            self.state = VoicemailState::Done;
            return Ok(AppAction::Hangup {
                reason: Some(CallRecordHangupReason::BySystem),
                code: None,
            });
        }
        Ok(AppAction::Continue)
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_greeting_path;

    #[test]
    fn greeting_param_wins_over_global() {
        assert_eq!(
            resolve_greeting_path(Some("sounds/dialplan.wav"), Some("sounds/global.wav")),
            "sounds/dialplan.wav"
        );
    }

    #[test]
    fn greeting_global_used_when_no_param() {
        assert_eq!(
            resolve_greeting_path(None, Some("sounds/global.wav")),
            "sounds/global.wav"
        );
    }

    #[test]
    fn greeting_falls_back_to_builtin_without_config() {
        assert_eq!(
            resolve_greeting_path(None, None),
            "sounds/voicemail/greeting.wav"
        );
    }

    #[test]
    fn greeting_blank_values_fall_through() {
        // Blank dialplan param -> global; blank global -> built-in.
        assert_eq!(
            resolve_greeting_path(Some("  "), Some("sounds/global.wav")),
            "sounds/global.wav"
        );
        assert_eq!(
            resolve_greeting_path(None, Some("")),
            "sounds/voicemail/greeting.wav"
        );
    }
}
