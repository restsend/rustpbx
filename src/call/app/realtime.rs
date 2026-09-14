//! Realtime (AI voice) application — inserts a bidirectional audio bridge to
//! an external realtime endpoint (OpenAI Realtime API or a raw-PCM16
//! self-hosted server) into the call.
//!
//! The app itself is deliberately thin: it answers the call, hands the
//! resolved [`RealtimeParams`] to the session (`CallCommand::RealtimeStart`)
//! and stays in the event loop while the session-side bridge pumps audio and
//! emits RWI `realtime_event`s (transcripts, function calls, barge-in).
//! On exit it stops the bridge.
//!
//! Startable from every app entry point with no extra plumbing:
//! - dialplan route: `action = "application"`, `app = "realtime"`
//! - outbound campaigns: `on_answer: {"type": "app", "app_name": "realtime"}`
//! - IVR node: `EntryAction::StartApp`
//! - RWI: `app.start`
//!
//! Credentials are resolved from `[[realtime]]` presets (or env fallback)
//! inside the session — app params reference a preset by `name` and can
//! never carry the API key.

use crate::call::app::{
    AppAction, ApplicationContext, CallApp, CallAppType, CallController,
};
use crate::call::realtime::RealtimeParams;
use async_trait::async_trait;
use tracing::info;

pub struct RealtimeApp {
    params: RealtimeParams,
}

impl RealtimeApp {
    pub fn new(params: RealtimeParams) -> Self {
        Self { params }
    }

    /// Resolve app params (JSON object) against `[[realtime]]` presets.
    pub fn from_params(
        value: Option<&serde_json::Value>,
        presets: Option<&[crate::config::RealtimePreset]>,
    ) -> anyhow::Result<Self> {
        let value = value
            .ok_or_else(|| anyhow::anyhow!("realtime app requires params (preset or url)"))?;
        Ok(Self::new(RealtimeParams::resolve(value, presets)?))
    }
}

#[async_trait]
impl CallApp for RealtimeApp {
    fn app_type(&self) -> CallAppType {
        CallAppType::Realtime
    }

    fn name(&self) -> &str {
        "realtime"
    }

    async fn on_enter(
        &mut self,
        ctrl: &mut CallController,
        _ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        info!(
            protocol = self.params.protocol.as_str(),
            hangup_on_disconnect = self.params.hangup_on_disconnect,
            "Realtime app entering"
        );
        ctrl.answer().await?;
        ctrl.start_realtime(self.params.clone())?;
        ctrl.record_trace(
            crate::call_errors::TraceEvent::new(
                crate::call_errors::TraceKind::Ivr,
                format!("Realtime bridge started ({})", self.params.protocol.as_str()),
            )
            .severity(crate::call_errors::ErrSeverity::Info),
        );
        // Stay in the event loop — the session-side bridge drives media and
        // emits events; the call ends when the endpoint closes (per config)
        // or the remote party hangs up.
        Ok(AppAction::Continue)
    }

    // NOTE: `on_exit` has no controller — the session owns the bridge
    // lifecycle and tears it down on hangup, app replacement (`StopApp` /
    // `StartApp`) and session teardown. See `SipSession::
    // teardown_realtime_bridge`.
}
