use crate::call::app::{
    AppAction, CallApp, CallAppType, CallController, ApplicationContext, RecordingInfo,
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
            greeting_path: "sounds/voicemail/greeting.wav".to_string(),
            state: VoicemailState::Greeting,
            recording_path: None,
        }
    }

    pub fn with_greeting_path(mut self, path: impl Into<String>) -> Self {
        self.greeting_path = path.into();
        self
    }
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
        ctrl.play_audio(&self.greeting_path, false).await?;
        Ok(AppAction::Continue)
    }

    async fn on_audio_complete(
        &mut self,
        _track_id: String,
        ctrl: &mut CallController,
        _ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        if self.state == VoicemailState::Greeting {
            self.state = VoicemailState::Recording;
            let path = format!("/tmp/voicemail_{}_{}.wav", self.extension, uuid::Uuid::new_v4());
            info!(path = %path, "Starting voicemail recording");
            ctrl.start_recording(&path, Some(Duration::from_secs(300)), true)
                .await?;
            self.recording_path = Some(path);
        }
        Ok(AppAction::Continue)
    }

    async fn on_dtmf(
        &mut self,
        digit: String,
        ctrl: &mut CallController,
        _ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        if digit == "#" && self.state == VoicemailState::Recording {
            info!("DTMF # received, stopping voicemail recording");
            ctrl.stop_recording().await.ok();
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
        _ctrl: &mut CallController,
        _ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        info!(
            path = %info.path,
            duration = ?info.duration,
            size = info.size_bytes,
            "Voicemail recording completed"
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
