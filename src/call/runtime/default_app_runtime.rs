//! Default AppRuntime implementation
//!
//! This module provides `DefaultAppRuntime` which wraps the existing
//! `CallApp` / `AppEventLoop` framework.

use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::call::app::{ApplicationContext, CallApp, CallController, ControllerEvent};
use crate::call::domain::MediaCapability;
use crate::proxy::proxy_call::state::{SessionAction, SipSessionHandle};

use super::{AppDescriptor, AppResult, AppRuntime, AppRuntimeError, AppStatus};

/// State for a running application
#[derive(Clone)]
struct RunningApp {
    name: String,
    cancel_token: CancellationToken,
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
    context: Arc<ApplicationContext>,
    /// Currently running app (if any)
    running: RwLock<Option<RunningApp>>,
    /// App factory function
    app_factory: Option<Arc<dyn AppFactory>>,
}

/// Factory trait for creating CallApp instances
#[async_trait::async_trait]
pub trait AppFactory: Send + Sync {
    fn create_app(
        &self,
        app_name: &str,
        params: Option<serde_json::Value>,
        context: &ApplicationContext,
    ) -> Option<Box<dyn CallApp>>;
}

impl DefaultAppRuntime {
    pub fn new(config: AppRuntimeConfig) -> Self {
        Self {
            session_id: config.session_id,
            handle: config.handle,
            context: config.context,
            running: RwLock::new(None),
            app_factory: None,
        }
    }

    pub fn with_factory(mut self, factory: Arc<dyn AppFactory>) -> Self {
        self.app_factory = Some(factory);
        self
    }

    /// Get app descriptor for known app types
    fn get_descriptor(&self, app_name: &str) -> AppDescriptor {
        match app_name {
            "ivr" => AppDescriptor::ivr(),
            "voicemail" => AppDescriptor::voicemail(),
            "queue" => AppDescriptor::queue(),
            _ => AppDescriptor::new(app_name).with_capabilities(vec![MediaCapability::Full]),
        }
    }
}

#[async_trait]
impl AppRuntime for DefaultAppRuntime {
    async fn start_app(
        &self,
        app_name: &str,
        params: Option<serde_json::Value>,
        auto_answer: bool,
    ) -> AppResult<()> {
        // Check if already running
        {
            let running = self.running.read().await;
            if running.is_some() {
                return Err(AppRuntimeError::AlreadyRunning(app_name.to_string()));
            }
        }

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
            factory.create_app(app_name, params.clone(), &self.context)
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

        // Store running state
        {
            let mut running = self.running.write().await;
            *running = Some(RunningApp {
                name: app_name.to_string(),
                cancel_token: cancel_token.clone(),
            });
        }

        // Auto-answer if requested
        if auto_answer {
            self.handle
                .send_command(SessionAction::AcceptCall {
                    callee: None,
                    sdp: None,
                    dialog_id: None,
                })
                .map_err(|e| AppRuntimeError::StartFailed(e.to_string()))?;
        }

        // Spawn the event loop
        let session_id_for_log = self.session_id.clone();
        let app_name_owned = app_name.to_string();
        let context = self.context.clone();
        let handle = self.handle.clone();

        tokio::spawn(async move {
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

            // Clear the app event sender so the session knows the app has exited.
            handle.set_app_event_sender(None);
            tracing::info!(
                "App {} exited for session {}",
                app_name_owned,
                session_id_for_log
            );
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

    fn status(&self) -> AppStatus {
        if self.is_running() {
            AppStatus::Running
        } else {
            AppStatus::Idle
        }
    }

    fn current_app(&self) -> Option<String> {
        if let Ok(guard) = self.running.try_read() {
            guard.as_ref().map(|r| r.name.clone())
        } else {
            None
        }
    }

    fn required_capabilities(&self) -> Vec<MediaCapability> {
        if let Ok(guard) = self.running.try_read()
            && let Some(running) = guard.as_ref() {
                let descriptor = self.get_descriptor(&running.name);
                return descriptor.required_capabilities;
            }
        vec![]
    }

    fn app_descriptor(&self, app_name: &str) -> Option<AppDescriptor> {
        Some(self.get_descriptor(app_name))
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
}
