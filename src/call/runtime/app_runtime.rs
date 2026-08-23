//! AppRuntime - Abstract application runtime interface
//!
//! This module provides the `AppRuntime` trait that abstracts the application
//! lifecycle management (IVR, Voicemail, Queue, etc.) from the session layer.

use async_trait::async_trait;

/// Application runtime error types
#[derive(Debug, thiserror::Error)]
pub enum AppRuntimeError {
    #[error("application not running")]
    NotRunning,

    #[error("application already running: {0}")]
    AlreadyRunning(String),

    #[error("unknown application: {0}")]
    UnknownApp(String),

    /// Application is known but could not start because of a configuration
    /// problem (e.g. a missing or parse-invalid IVR config file). Carries the
    /// detailed, operator-actionable reason.
    #[error("application configuration error: {0}")]
    ConfigError(String),

    #[error("failed to start application: {0}")]
    StartFailed(String),

    #[error("failed to inject event: {0}")]
    InjectFailed(String),

    #[error("application error: {0}")]
    AppError(#[from] anyhow::Error),
}

/// Result type for AppRuntime operations
pub type AppResult<T> = Result<T, AppRuntimeError>;

/// Application runtime trait
///
/// This trait abstracts the application lifecycle management from the session layer.
/// Implementations manage the actual app instance and event routing.
#[async_trait]
pub trait AppRuntime: Send + Sync {
    /// Access the shared application context (variables, pending app state).
    /// Returns `None` for runtimes without a context; consumers fall back to
    /// their default behavior instead of downcasting.
    fn app_context(&self) -> Option<&std::sync::Arc<crate::call::app::ApplicationContext>> {
        None
    }

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

    /// Get the name of the currently running application (if any)
    fn current_app(&self) -> Option<String>;

    /// Synchronous best-effort cancellation of the running app's event loop.
    ///
    /// Used by `SipSession::drop` (which cannot `.await`) so an orphaned app
    /// loop cannot outlive the session on abnormal teardown (task abort/panic).
    /// Default no-op for runtimes that don't support sync cancellation.
    fn cancel_sync(&self) {}
}
