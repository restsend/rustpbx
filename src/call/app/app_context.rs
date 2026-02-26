use crate::config::Config;
use chrono::{DateTime, Utc};
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Metadata about the current call, derived from the SIP INVITE.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallInfo {
    /// Unique session identifier (matches `CallSession::session_id`).
    pub session_id: String,
    /// Caller number/URI (From header).
    pub caller: String,
    /// Callee number/URI (Request-URI or To header).
    pub callee: String,
    /// Call direction.
    pub direction: String,
    /// When the session started.
    pub started_at: DateTime<Utc>,
}

pub struct AppSharedState {
    /// Arbitrary typed data, keyed by string.
    ///
    /// Use this to share state between addons (e.g., conference rooms, queue stats).
    pub custom_data: Arc<RwLock<HashMap<String, Box<dyn std::any::Any + Send + Sync>>>>,
}

impl AppSharedState {
    pub fn new() -> Self {
        Self {
            custom_data: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for AppSharedState {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for AppSharedState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppSharedState").finish()
    }
}

/// The application context, providing access to shared resources.
///
/// Passed (by reference) to every [`CallApp`] event handler. Contains the database
/// connection, storage backend, HTTP client, call info, and system configuration.
///
/// # Example
///
/// ```rust,ignore
/// async fn on_enter(
///     &mut self,
///     ctrl: &mut CallController,
///     ctx: &ApplicationContext,
/// ) -> Result<AppAction> {
///     // Access the database
///     let db = &ctx.db;
///     
///     // Access call metadata
///     let caller = &ctx.call_info.caller;
///     tracing::info!("Call from {}", caller);
///     
///     // Read/write session variables
///     let vars = ctx.session_vars.read().await;
///     if let Some(lang) = vars.get("language") {
///         tracing::info!("Language: {}", lang);
///     }
///
///     Ok(AppAction::Continue)
/// }
/// ```
#[derive(Clone)]
pub struct ApplicationContext {
    /// Session-level variables shared across chained applications.
    pub session_vars: Arc<RwLock<HashMap<String, String>>>,

    /// Global shared state (accessible from any app instance).
    pub shared_state: Arc<AppSharedState>,

    /// Database connection (SeaORM).
    pub db: DatabaseConnection,

    /// HTTP client for outbound requests.
    pub http_client: reqwest::Client,

    /// Call metadata.
    pub call_info: CallInfo,

    /// System configuration.
    pub config: Arc<Config>,

    /// Storage backend for file operations (recordings, greetings, etc.).
    pub storage: crate::storage::Storage,
}

impl ApplicationContext {
    /// Create a new application context.
    pub fn new(
        db: DatabaseConnection,
        call_info: CallInfo,
        config: Arc<Config>,
        storage: crate::storage::Storage,
    ) -> Self {
        Self {
            session_vars: Arc::new(RwLock::new(HashMap::new())),
            shared_state: Arc::new(AppSharedState::new()),
            db,
            http_client: reqwest::Client::new(),
            call_info,
            config,
            storage,
        }
    }

    /// Set a session variable.
    pub async fn set_var(&self, key: impl Into<String>, value: impl Into<String>) {
        let mut vars = self.session_vars.write().await;
        vars.insert(key.into(), value.into());
    }

    /// Get a session variable.
    pub async fn get_var(&self, key: &str) -> Option<String> {
        let vars = self.session_vars.read().await;
        vars.get(key).cloned()
    }
}

impl std::fmt::Debug for ApplicationContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApplicationContext")
            .field("call_info", &self.call_info)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_call_info() -> CallInfo {
        CallInfo {
            session_id: "test-session-1".to_string(),
            caller: "sip:alice@example.com".to_string(),
            callee: "sip:bob@example.com".to_string(),
            direction: "inbound".to_string(),
            started_at: Utc::now(),
        }
    }

    #[test]
    fn test_call_info_serialization() {
        let info = make_call_info();
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("test-session-1"));
        assert!(json.contains("alice"));
    }

    #[test]
    fn test_shared_state_default() {
        let state = AppSharedState::default();
        let debug = format!("{:?}", state);
        assert!(debug.contains("AppSharedState"));
    }

    #[tokio::test]
    async fn test_session_vars() {
        let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
        let storage_config = crate::storage::StorageConfig::Local {
            path: std::env::temp_dir()
                .join("rustpbx-test-storage")
                .to_string_lossy()
                .to_string(),
        };
        let storage = crate::storage::Storage::new(&storage_config).unwrap();
        let ctx =
            ApplicationContext::new(db, make_call_info(), Arc::new(Config::default()), storage);

        // Initially empty
        assert!(ctx.get_var("lang").await.is_none());

        // Set and get
        ctx.set_var("lang", "zh").await;
        assert_eq!(ctx.get_var("lang").await, Some("zh".to_string()));

        // Overwrite
        ctx.set_var("lang", "en").await;
        assert_eq!(ctx.get_var("lang").await, Some("en".to_string()));
    }
}
