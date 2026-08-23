//! Default AppRuntime implementation
//!
//! This module provides `DefaultAppRuntime` which wraps the existing
//! `CallApp` / `AppEventLoop` framework.

use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::call::app::{ApplicationContext, CallApp, CallController, ControllerEvent};
use crate::call::domain::CallCommand;
use crate::proxy::proxy_call::sip_session::SipSessionHandle;

use super::{AppResult, AppRuntime, AppRuntimeError};

/// State for a running application
#[derive(Clone)]
struct RunningApp {
    name: String,
    cancel_token: CancellationToken,
    /// Generation claimed by the `start_app` that installed this slot.
    /// Lets the event-loop teardown clear its own registration on natural
    /// exit without ever clobbering a successor's.
    generation: u64,
}

/// Configuration needed to create an AppRuntime
pub struct AppRuntimeConfig {
    pub session_id: String,
    pub handle: SipSessionHandle,
    pub context: Arc<ApplicationContext>,
}

/// Default implementation of AppRuntime using the existing CallApp framework
pub struct DefaultAppRuntime {
    session_id: String,
    handle: SipSessionHandle,
    /// Shared per-call context (public for post-call hook access).
    pub context: Arc<ApplicationContext>,
    /// Currently running app (if any). Shared with the event-loop teardown
    /// task so a naturally-exited app can clear its own registration.
    running: Arc<RwLock<Option<RunningApp>>>,
    /// App factory function
    app_factory: Option<Arc<dyn AppFactory>>,
    /// Incremented on every successful `start_app`.
    app_generation: Arc<AtomicU64>,
}

/// Factory trait for creating CallApp instances.
///
/// `Ok(None)` means the factory does not handle `app_name` (→ "unknown
/// application"). `Err(detail)` means the app is known but failed to start for
/// a concrete reason (e.g. a missing IVR config file) — the detail is surfaced
/// to the caller via [`AppRuntimeError::ConfigError`].
#[async_trait::async_trait]
pub trait AppFactory: Send + Sync {
    async fn create_app(
        &self,
        app_name: &str,
        params: Option<serde_json::Value>,
        context: &ApplicationContext,
    ) -> Result<Option<Box<dyn CallApp>>, anyhow::Error>;
}
impl DefaultAppRuntime {
    pub fn new(config: AppRuntimeConfig) -> Self {
        Self {
            session_id: config.session_id,
            handle: config.handle,
            context: config.context,
            running: Arc::new(RwLock::new(None)),
            app_factory: None,
            app_generation: Arc::new(AtomicU64::new(0)),
        }
    }
    pub fn with_factory(mut self, factory: Arc<dyn AppFactory>) -> Self {
        self.app_factory = Some(factory);
        self
    }
}

impl Drop for DefaultAppRuntime {
    fn drop(&mut self) {
        // Best-effort cancel of any running app so its spawned AppEventLoop
        // task stops promptly instead of leaking when the runtime is dropped
        // without an explicit `stop_app()`. The RwLock is a tokio lock, so we
        // use try_write() here (Drop cannot await). If the lock is contended
        // the app's event channel will still close when the session handle is
        // dropped, which also terminates the loop.
        if let Ok(mut running) = self.running.try_write() {
            if let Some(app) = running.take() {
                app.cancel_token.cancel();
            }
        }
    }
}

#[async_trait]
impl AppRuntime for DefaultAppRuntime {
    fn app_context(&self) -> Option<&std::sync::Arc<crate::call::app::ApplicationContext>> {
        Some(&self.context)
    }

    async fn start_app(
        &self,
        app_name: &str,
        params: Option<serde_json::Value>,
        auto_answer: bool,
    ) -> AppResult<()> {
        // Check if already running
        {
            let running = self.running.read().await;
            if let Some(current) = running.as_ref() {
                // Report the app that actually occupies the slot — callers
                // (e.g. the CSAT hook) branch on this name.
                return Err(AppRuntimeError::AlreadyRunning(current.name.clone()));
            }
        }

        // Claim the next generation *before* installing the event sender.
        // Transfer → stop_app → start_app races with the predecessor event-loop
        // teardown: if we only bump after `create_app` awaits, the predecessor
        // can still see its own generation as current and call
        // `set_app_event_sender(None)`, dropping the successor's channel and
        // killing the new IVR with ExitReason::Normal.
        let generation = self.app_generation.fetch_add(1, Ordering::SeqCst) + 1;

        // Create event channel for app events (DTMF, hangup, etc.)
        let (event_tx, event_rx) = mpsc::unbounded_channel::<ControllerEvent>();

        // Create controller — it owns the timer sender, we get the receiver back.
        let (controller, timer_rx) = CallController::new(self.handle.clone(), event_rx);

        // Register the event sender with the session so SipSession can forward
        // DTMF / hangup / audio-complete events to the running app.
        self.handle.set_app_event_sender(Some(event_tx.clone()));

        // Create cancel token
        let cancel_token = CancellationToken::new();

        // Get the app from factory
        let app = if let Some(factory) = &self.app_factory {
            match factory
                .create_app(app_name, params.clone(), &self.context)
                .await
            {
                Ok(app) => app,
                // The app is known but failed to start (e.g. missing IVR config).
                // Surface the specific reason instead of a generic "unknown app".
                Err(e) => {
                    self.handle.set_app_event_sender(None);
                    return Err(AppRuntimeError::ConfigError(e.to_string()));
                }
            }
        } else {
            None
        };

        let app = match app {
            Some(app) => app,
            None => {
                self.handle.set_app_event_sender(None);
                return Err(AppRuntimeError::UnknownApp(app_name.to_string()));
            }
        };

        {
            let mut running = self.running.write().await;
            *running = Some(RunningApp {
                name: app_name.to_string(),
                cancel_token: cancel_token.clone(),
                generation,
            });
        }

        // Auto-answer if requested
        if auto_answer {
            self.handle
                .send_command(CallCommand::Answer {
                    leg_id: crate::call::domain::LegId::from("caller"),
                })
                .map_err(|e| AppRuntimeError::StartFailed(e.to_string()))?;
        }

        // Spawn the event loop
        let session_id_for_log = self.session_id.clone();
        let app_name_owned = app_name.to_string();
        let context = self.context.clone();
        let handle = self.handle.clone();
        let generation_counter = self.app_generation.clone();
        let running_slot = self.running.clone();

        crate::utils::spawn(async move {
            let event_loop = crate::call::app::AppEventLoop::new(
                app,
                controller,
                (*context).clone(),
                cancel_token,
                timer_rx,
            );

            if let Err(e) = event_loop.run().await {
                tracing::error!(
                    "App {} failed for session {}: {}",
                    app_name_owned,
                    session_id_for_log,
                    e
                );
            }

            // A Transfer that starts a successor app (JumpIvr / toivr) runs
            // `start_app` before this task resumes. Clearing the event sender
            // unconditionally would drop the successor's DTMF/timeout channel
            // and kill the new IVR immediately. Only tear down when we are
            // still the current generation.
            let still_current = generation_counter.load(Ordering::SeqCst) == generation;
            if still_current {
                // Clear the running registration on natural exit (e.g.
                // QueueApp's Exit once the agent answers) so the next app
                // starts cleanly instead of tripping AlreadyRunning and
                // requiring the stop+restart recovery. The identity check
                // under the write lock makes it impossible to clear a
                // successor's slot if one was installed between the
                // generation load above and here.
                {
                    let mut guard = running_slot.write().await;
                    if guard.as_ref().map(|app| app.generation) == Some(generation) {
                        *guard = None;
                    }
                }

                handle.set_app_event_sender(None);

                // Notify the session that the app has exited so it can run
                // post-exit hooks (e.g. IVR-exec unhold + result delivery).
                // Skip when a successor app already replaced us (Transfer /
                // JumpIvr) — that generation owns the session now.
                if let Err(e) = handle.send_command(CallCommand::AppExited) {
                    tracing::warn!(
                        "Failed to send AppExited for session {}: {}",
                        session_id_for_log,
                        e
                    );
                }
            }
        });

        tracing::info!("App {} started for session {}", app_name, self.session_id);
        Ok(())
    }

    async fn stop_app(&self, reason: Option<String>) -> AppResult<()> {
        let running = {
            let running = self.running.read().await;
            running.clone()
        };

        match running {
            Some(running) => {
                // Cancel the event loop
                running.cancel_token.cancel();

                // Clear running state
                {
                    let mut running_guard = self.running.write().await;
                    *running_guard = None;
                }

                tracing::info!(
                    "App {} stopped for session {}: {}",
                    running.name,
                    self.session_id,
                    reason.unwrap_or_else(|| "no reason".to_string())
                );

                Ok(())
            }
            None => Err(AppRuntimeError::NotRunning),
        }
    }

    fn inject_event(&self, event: serde_json::Value) -> AppResult<()> {
        // Parse the event type and convert to ControllerEvent
        let controller_event = parse_json_event(&event)?;

        // Try to send via the handle's send_app_event
        if self.handle.send_app_event(controller_event) {
            Ok(())
        } else {
            Err(AppRuntimeError::InjectFailed(
                "no app running or channel closed".to_string(),
            ))
        }
    }

    fn is_running(&self) -> bool {
        // Check if there's an app event sender set
        // This is a quick synchronous check
        if let Ok(guard) = self.running.try_read() {
            guard.is_some()
        } else {
            false
        }
    }

    fn cancel_sync(&self) {
        if let Ok(guard) = self.running.try_read()
            && let Some(running) = guard.as_ref()
        {
            running.cancel_token.cancel();
        }
    }

    fn current_app(&self) -> Option<String> {
        if let Ok(guard) = self.running.try_read() {
            guard.as_ref().map(|r| r.name.clone())
        } else {
            None
        }
    }
}

/// Parse a JSON event into a ControllerEvent
fn parse_json_event(value: &serde_json::Value) -> AppResult<ControllerEvent> {
    let obj = value
        .as_object()
        .ok_or_else(|| AppRuntimeError::InjectFailed("event must be a JSON object".to_string()))?;

    let event_type = obj.get("type").and_then(|v| v.as_str()).ok_or_else(|| {
        AppRuntimeError::InjectFailed("event must have a 'type' field".to_string())
    })?;

    match event_type {
        "dtmf" => {
            let digit = obj
                .get("digit")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Ok(ControllerEvent::DtmfReceived(digit))
        }
        "audio_complete" => {
            let track_id = obj
                .get("track_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let interrupted = obj
                .get("interrupted")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            Ok(ControllerEvent::AudioComplete {
                track_id,
                interrupted,
            })
        }
        "recording_complete" => {
            let path = obj
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Ok(ControllerEvent::RecordingComplete(
                crate::call::app::RecordingInfo {
                    path,
                    duration: std::time::Duration::from_secs(0),
                    size_bytes: 0,
                },
            ))
        }
        "hangup" => {
            let _reason = obj.get("reason").and_then(|v| v.as_str());
            // Note: CallRecordHangupReason doesn't have FromStr, so we just use None
            Ok(ControllerEvent::Hangup(None))
        }
        "timeout" => {
            let timer_id = obj
                .get("timer_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Ok(ControllerEvent::Timeout(timer_id))
        }
        "custom" => {
            let name = obj
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let data = obj.get("data").cloned().unwrap_or(serde_json::Value::Null);
            Ok(ControllerEvent::Custom(name, data))
        }
        _ => Err(AppRuntimeError::InjectFailed(format!(
            "unknown event type: {}",
            event_type
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dtmf_event() {
        let json = serde_json::json!({
            "type": "dtmf",
            "digit": "5"
        });
        let event = parse_json_event(&json).unwrap();
        assert!(matches!(event, ControllerEvent::DtmfReceived(d) if d == "5"));
    }

    #[test]
    fn test_parse_audio_complete_event() {
        let json = serde_json::json!({
            "type": "audio_complete",
            "track_id": "track-123",
            "interrupted": true
        });
        let event = parse_json_event(&json).unwrap();
        if let ControllerEvent::AudioComplete {
            track_id,
            interrupted,
        } = event
        {
            assert_eq!(track_id, "track-123");
            assert!(interrupted);
        } else {
            panic!("Expected AudioComplete");
        }
    }

    #[test]
    fn test_parse_custom_event() {
        let json = serde_json::json!({
            "type": "custom",
            "name": "webhook",
            "data": {"action": "transfer", "target": "1001"}
        });
        let event = parse_json_event(&json).unwrap();
        if let ControllerEvent::Custom(name, data) = event {
            assert_eq!(name, "webhook");
            assert_eq!(data["action"], "transfer");
        } else {
            panic!("Expected Custom");
        }
    }

    #[test]
    fn test_parse_unknown_event() {
        let json = serde_json::json!({
            "type": "unknown"
        });
        let result = parse_json_event(&json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_missing_type() {
        let json = serde_json::json!({
            "digit": "5"
        });
        let result = parse_json_event(&json);
        assert!(result.is_err());
    }

    // ── running-slot lifecycle ────────────────────────────────────────────

    /// An app that exits immediately from `on_enter`.
    struct ExitApp;

    #[async_trait::async_trait]
    impl crate::call::app::CallApp for ExitApp {
        fn app_type(&self) -> crate::call::app::CallAppType {
            crate::call::app::CallAppType::Custom
        }

        fn name(&self) -> &str {
            "exit_app"
        }

        async fn on_enter(
            &mut self,
            _controller: &mut crate::call::app::CallController,
            _context: &crate::call::app::ApplicationContext,
        ) -> anyhow::Result<crate::call::app::AppAction> {
            Ok(crate::call::app::AppAction::Exit)
        }
    }

    struct ExitAppFactory;

    #[async_trait]
    impl AppFactory for ExitAppFactory {
        async fn create_app(
            &self,
            _app_name: &str,
            _params: Option<serde_json::Value>,
            _context: &crate::call::app::ApplicationContext,
        ) -> Result<Option<Box<dyn crate::call::app::CallApp>>, anyhow::Error> {
            Ok(Some(Box::new(ExitApp)))
        }
    }

    fn make_runtime() -> (DefaultAppRuntime, mpsc::Receiver<CallCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let handle = SipSessionHandle::new_for_test("runtime-test", cmd_tx);
        let call_info = crate::call::app::CallInfo {
            session_id: "runtime-test".into(),
            caller: "1001".into(),
            callee: "1002".into(),
            direction: "inbound".into(),
            started_at: chrono::Utc::now(),
            sip_headers: Default::default(),
            route_name: None,
        };
        let context = crate::call::app::ApplicationContext::new(
            Default::default(),
            call_info,
            std::sync::Arc::new(crate::config::Config::default()),
        );
        let runtime = DefaultAppRuntime::new(AppRuntimeConfig {
            session_id: "runtime-test".into(),
            handle,
            context: std::sync::Arc::new(context),
        })
        .with_factory(std::sync::Arc::new(ExitAppFactory));
        (runtime, cmd_rx)
    }

    /// Natural exit must clear the `running` slot so the next `start_app`
    /// succeeds cleanly instead of tripping `AlreadyRunning` (the stop+
    /// restart recovery in `SipSession::ensure_app_running` remains only as
    /// a race fallback). Regression for the "runtime still marked running,
    /// restarting app" warn on every app transition.
    #[tokio::test]
    async fn natural_exit_clears_running_slot() {
        let (runtime, mut cmd_rx) = make_runtime();

        runtime
            .start_app("exit_app", None, false)
            .await
            .expect("first start must succeed");
        assert_eq!(runtime.current_app().as_deref(), Some("exit_app"));

        // Wait (bounded) for the event loop to run, exit and clear the slot.
        let mut cleared = false;
        for _ in 0..100 {
            if runtime.current_app().is_none() {
                cleared = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(cleared, "running slot must be cleared after natural exit");
        assert!(!runtime.is_running());

        // The teardown also notified the session with AppExited.
        let mut got_app_exited = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            if matches!(cmd, CallCommand::AppExited) {
                got_app_exited = true;
            }
        }
        assert!(got_app_exited, "AppExited must be sent on natural exit");

        // Restarting after a natural exit must not hit AlreadyRunning.
        runtime
            .start_app("exit_app", None, false)
            .await
            .expect("restart after natural exit must succeed directly");
    }

    /// An explicit `stop_app` still clears the slot immediately and cancels
    /// the app (unchanged legacy behavior).
    #[tokio::test]
    async fn stop_app_still_clears_slot() {
        let (runtime, _cmd_rx) = make_runtime();
        runtime
            .start_app("exit_app", None, false)
            .await
            .expect("start must succeed");
        runtime
            .stop_app(Some("test".into()))
            .await
            .expect("stop must succeed");
        assert!(runtime.current_app().is_none());
        assert!(runtime.stop_app(None).await.is_err(), "second stop: NotRunning");
    }
}
