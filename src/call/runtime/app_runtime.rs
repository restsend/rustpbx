//! AppRuntime - Abstract application runtime interface
//!
//! This module provides the `AppRuntime` trait that abstracts the application
//! lifecycle management (IVR, Voicemail, Queue, etc.) from the session layer.
//!
//! ## Design Goals
//!
//! 1. **Decoupling**: Separate app lifecycle from session state machine
//! 2. **Capability Gate**: Check media requirements before starting apps
//! 3. **Event Safety**: Ensure events are delivered correctly during app lifetime
//! 4. **Lifecycle Management**: Clean startup and shutdown
//!
//! ## Architecture
//!
//! ```text
//! ┌────────────────────────────────────────────────────────────────┐
//! │                     Session Layer                              │
//! │   CallSession                                                  │
//! └────────────┬───────────────────────────────────────────────────┘
//!              │ manages
//!              ▼
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                     AppRuntime                                  │
//! │   - start_app(name, params) -> Result<()>                       │
//! │   - stop_app(reason) -> Result<()>                              │
//! │   - inject_event(event) -> Result<()>                           │
//! │   - is_running() -> bool                                        │
//! │   - required_capabilities() -> Vec<MediaCapability>             │
//! └────────────┬────────────────────────────────────────────────────┘
//!              │ drives
//!              ▼
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                     CallApp Implementation                      │
//! │   IvrApp │ VoicemailApp │ QueueApp │ ConferenceApp │ CustomApp  │
//! └─────────────────────────────────────────────────────────────────┘
//! ```

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::call::domain::MediaCapability;

/// Application runtime error types
#[derive(Debug, thiserror::Error)]
pub enum AppRuntimeError {
    #[error("application not running")]
    NotRunning,

    #[error("application already running: {0}")]
    AlreadyRunning(String),

    #[error("unknown application: {0}")]
    UnknownApp(String),

    #[error("media capability not satisfied: {0:?}")]
    CapabilityNotSatisfied(Vec<MediaCapability>),

    #[error("failed to start application: {0}")]
    StartFailed(String),

    #[error("failed to inject event: {0}")]
    InjectFailed(String),

    #[error("failed to stop application: {0}")]
    StopFailed(String),

    #[error("application error: {0}")]
    AppError(#[from] anyhow::Error),
}

/// Result type for AppRuntime operations
pub type AppResult<T> = Result<T, AppRuntimeError>;

/// Application status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum AppStatus {
    /// No application is running
    #[default]
    Idle,
    /// Application is starting up
    Starting,
    /// Application is running normally
    Running,
    /// Application is in the process of stopping
    Stopping,
    /// Application has stopped
    Stopped,
    /// Application failed with an error
    Failed,
}


impl std::fmt::Display for AppStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppStatus::Idle => write!(f, "idle"),
            AppStatus::Starting => write!(f, "starting"),
            AppStatus::Running => write!(f, "running"),
            AppStatus::Stopping => write!(f, "stopping"),
            AppStatus::Stopped => write!(f, "stopped"),
            AppStatus::Failed => write!(f, "failed"),
        }
    }
}

/// Application descriptor - metadata about an app
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppDescriptor {
    /// Application name (e.g., "ivr", "voicemail", "queue")
    pub name: String,
    /// Human-readable description
    pub description: Option<String>,
    /// Required media capabilities
    pub required_capabilities: Vec<MediaCapability>,
    /// Whether the app can handle bypass mode (no media anchoring)
    pub supports_bypass: bool,
    /// Application version
    pub version: Option<String>,
}

impl AppDescriptor {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            required_capabilities: Vec::new(),
            supports_bypass: false,
            version: None,
        }
    }

    /// Create an IVR app descriptor
    pub fn ivr() -> Self {
        Self {
            name: "ivr".to_string(),
            description: Some("Interactive Voice Response".to_string()),
            required_capabilities: vec![MediaCapability::Full],
            supports_bypass: false,
            version: None,
        }
    }

    /// Create a voicemail app descriptor
    pub fn voicemail() -> Self {
        Self {
            name: "voicemail".to_string(),
            description: Some("Voicemail recording and playback".to_string()),
            required_capabilities: vec![MediaCapability::Full],
            supports_bypass: false,
            version: None,
        }
    }

    /// Create a queue app descriptor
    pub fn queue() -> Self {
        Self {
            name: "queue".to_string(),
            description: Some("Call queue with hold music".to_string()),
            required_capabilities: vec![MediaCapability::Full],
            supports_bypass: false,
            version: None,
        }
    }

    /// Create a simple signaling app descriptor (works in bypass mode)
    pub fn signaling_only() -> Self {
        Self {
            name: "signaling".to_string(),
            description: Some("Signaling-only application".to_string()),
            required_capabilities: vec![],
            supports_bypass: true,
            version: None,
        }
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    pub fn with_capabilities(mut self, caps: Vec<MediaCapability>) -> Self {
        self.required_capabilities = caps;
        self
    }

    pub fn with_bypass_support(mut self, supports: bool) -> Self {
        self.supports_bypass = supports;
        self
    }

    /// Check if the given capabilities satisfy this app's requirements
    pub fn check_capabilities(&self, available: &[MediaCapability]) -> CapabilityCheckResult {
        if self.required_capabilities.is_empty() {
            return CapabilityCheckResult::Satisfied;
        }

        // Check if all required capabilities are available
        // Full capability satisfies any requirement
        // Limited satisfies Limited and SignalingOnly
        // SignalingOnly only satisfies SignalingOnly
        let missing: Vec<_> = self
            .required_capabilities
            .iter()
            .filter(|req| {
                !available.iter().any(|avail| {
                    matches!(
                        (avail, req),
                        (MediaCapability::Full, _)
                            | (MediaCapability::Limited, MediaCapability::Limited)
                            | (MediaCapability::Limited, MediaCapability::SignalingOnly)
                            | (
                                MediaCapability::SignalingOnly,
                                MediaCapability::SignalingOnly
                            )
                    )
                })
            })
            .cloned()
            .collect();

        if missing.is_empty() {
            CapabilityCheckResult::Satisfied
        } else {
            CapabilityCheckResult::Missing(missing)
        }
    }
}

/// Result of capability check
#[derive(Debug, Clone)]
pub enum CapabilityCheckResult {
    /// All required capabilities are available
    Satisfied,
    /// Some capabilities are missing
    Missing(Vec<MediaCapability>),
}

impl CapabilityCheckResult {
    pub fn is_satisfied(&self) -> bool {
        matches!(self, CapabilityCheckResult::Satisfied)
    }
}

/// Application runtime trait
///
/// This trait abstracts the application lifecycle management from the session layer.
/// Implementations manage the actual app instance and event routing.
#[async_trait]
pub trait AppRuntime: Send + Sync {
    /// Start an application
    ///
    /// # Arguments
    /// * `app_name` - Name of the application to start
    /// * `params` - Optional parameters for the application
    /// * `auto_answer` - Whether to automatically answer the call
    ///
    /// # Returns
    /// * `Ok(())` - Application started successfully
    /// * `Err(AppRuntimeError)` - Start failed
    async fn start_app(
        &self,
        app_name: &str,
        params: Option<serde_json::Value>,
        auto_answer: bool,
    ) -> AppResult<()>;

    /// Stop the current application
    ///
    /// # Arguments
    /// * `reason` - Optional reason for stopping
    ///
    /// # Returns
    /// * `Ok(())` - Application stopped successfully
    /// * `Err(AppRuntimeError)` - Stop failed
    async fn stop_app(&self, reason: Option<String>) -> AppResult<()>;

    /// Inject an event into the running application
    ///
    /// # Arguments
    /// * `event` - The event to inject (as JSON for flexibility)
    ///
    /// # Returns
    /// * `Ok(())` - Event injected successfully
    /// * `Err(AppRuntimeError)` - Injection failed (e.g., no app running)
    fn inject_event(&self, event: serde_json::Value) -> AppResult<()>;

    /// Check if an application is currently running
    fn is_running(&self) -> bool;

    /// Get the current application status
    fn status(&self) -> AppStatus;

    /// Get the name of the currently running application (if any)
    fn current_app(&self) -> Option<String>;

    /// Get the required media capabilities for the current (or specified) app
    fn required_capabilities(&self) -> Vec<MediaCapability>;

    /// Get the application descriptor for a given app name
    fn app_descriptor(&self, app_name: &str) -> Option<AppDescriptor>;
}

/// Application factory trait - creates AppRuntime instances
#[async_trait]
pub trait AppRuntimeFactory: Send + Sync {
    /// Create a new AppRuntime instance for a session
    fn create_runtime(
        &self,
        session_id: &str,
        handle: crate::proxy::proxy_call::state::SipSessionHandle,
    ) -> Arc<dyn AppRuntime>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_descriptor_ivr() {
        let desc = AppDescriptor::ivr();
        assert_eq!(desc.name, "ivr");
        assert!(!desc.supports_bypass);
        assert!(!desc.required_capabilities.is_empty());
    }

    #[test]
    fn app_descriptor_voicemail() {
        let desc = AppDescriptor::voicemail();
        assert_eq!(desc.name, "voicemail");
        assert!(!desc.supports_bypass);
    }

    #[test]
    fn app_descriptor_signaling() {
        let desc = AppDescriptor::signaling_only();
        assert!(desc.supports_bypass);
        assert!(desc.required_capabilities.is_empty());
    }

    #[test]
    fn capability_check_satisfied() {
        let desc = AppDescriptor::signaling_only();
        let result = desc.check_capabilities(&[]);
        assert!(result.is_satisfied());
    }

    #[test]
    fn capability_check_missing() {
        let desc = AppDescriptor::ivr(); // Requires Full capability
        let result = desc.check_capabilities(&[MediaCapability::Limited]);
        assert!(!result.is_satisfied());
    }

    #[test]
    fn app_status_display() {
        assert_eq!(AppStatus::Idle.to_string(), "idle");
        assert_eq!(AppStatus::Running.to_string(), "running");
        assert_eq!(AppStatus::Failed.to_string(), "failed");
    }
}
