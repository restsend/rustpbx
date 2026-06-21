use crate::call::app::{AppAction, ApplicationContext, CallApp, CallAppType, CallController};
use async_trait::async_trait;

/// A minimal conference call app.
///
/// This app answers the call and keeps the session alive. The actual
/// conference mixing is set up by [`SipSession`] after the app starts
/// (see `DialplanFlow::Application` handler for `"conference"`).
pub struct ConferenceApp {
    conference_id: String,
}

impl ConferenceApp {
    pub fn new(conference_id: String, _caller_id: String) -> Self {
        Self { conference_id }
    }

    pub fn conference_id(&self) -> &str {
        &self.conference_id
    }
}

#[async_trait]
impl CallApp for ConferenceApp {
    fn app_type(&self) -> CallAppType {
        CallAppType::Conference
    }

    fn name(&self) -> &str {
        "conference"
    }

    async fn on_enter(
        &mut self,
        ctrl: &mut CallController,
        _ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        ctrl.answer().await?;
        Ok(AppAction::Continue)
    }

    async fn on_dtmf(
        &mut self,
        digit: String,
        _ctrl: &mut CallController,
        _ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        // # exits the conference, other digits are ignored.
        if digit == "#" {
            return Ok(AppAction::Hangup {
                reason: None,
                code: None,
            });
        }
        Ok(AppAction::Continue)
    }
}
