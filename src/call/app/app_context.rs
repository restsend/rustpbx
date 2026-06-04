use crate::call::app::CallApp;
use crate::call::app::ivr::trace::IvrTraceCollector;
use crate::call::runtime::PostCallHook;
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
    /// All SIP headers from the original INVITE (excluding standard transport
    /// headers like Via, Max-Forwards, Call-ID, CSeq, Content-Length).
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub sip_headers: HashMap<String, String>,
    /// Name of the matched routing rule that dispatched this call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_name: Option<String>,
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

    /// Queue name set by QueueApp — used by post-call hooks (e.g., CSAT survey).
    pub queue_name: Arc<RwLock<Option<String>>>,

    /// Database connection (SeaORM).
    pub db: DatabaseConnection,

    /// HTTP client for outbound requests.
    pub http_client: reqwest::Client,

    /// Call metadata.
    pub call_info: CallInfo,

    /// System configuration.
    pub config: Arc<Config>,

    /// RWI gateway for emitting real-time events.
    pub rwi_gateway: Option<crate::rwi::RwiGatewayRef>,

    /// IVR step trace collector for debugging (optional).
    pub ivr_trace: Option<Arc<IvrTraceCollector>>,

    /// Custom call app factories (registered by addons).
    pub app_factories: Arc<
        Vec<(
            &'static str,
            Arc<
                dyn Fn(
                        &str,
                        Option<serde_json::Value>,
                        &ApplicationContext,
                    ) -> Option<Box<dyn CallApp>>
                    + Send
                    + Sync,
            >,
        )>,
    >,

    /// Post-call hook (registered by callcenter addon).
    pub post_call_hook: Option<Arc<dyn PostCallHook>>,
}

impl ApplicationContext {
    /// Create a new application context.
    pub fn new(db: DatabaseConnection, call_info: CallInfo, config: Arc<Config>) -> Self {
        Self {
            session_vars: Arc::new(RwLock::new(HashMap::new())),
            queue_name: Arc::new(RwLock::new(None)),
            db,
            http_client: reqwest::Client::new(),
            call_info,
            config,
            rwi_gateway: None,
            ivr_trace: None,
            app_factories: Arc::new(Vec::new()),
            post_call_hook: None,
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

    /// Set the queue name for this session (called by QueueApp on enter).
    pub async fn set_queue_name(&self, name: impl Into<String>) {
        *self.queue_name.write().await = Some(name.into());
    }

    /// Get a usable database connection reference, or None if disconnected.
    pub fn db_connection(&self) -> Option<&DatabaseConnection> {
        match &self.db {
            DatabaseConnection::Disconnected => None,
            conn => Some(conn),
        }
    }
}

/// Extract all SIP headers from a request into a `HashMap`, skipping standard
/// transport/dialog headers that already have typed representations.
pub fn extract_sip_headers(request: &rsipstack::sip::Request) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    for h in request.headers.iter() {
        let skip = matches!(
            h,
            rsipstack::sip::Header::Via(_)
                | rsipstack::sip::Header::MaxForwards(_)
                | rsipstack::sip::Header::CallId(_)
                | rsipstack::sip::Header::CSeq(_)
                | rsipstack::sip::Header::ContentLength(_)
                | rsipstack::sip::Header::ContentType(_)
                | rsipstack::sip::Header::From(_)
                | rsipstack::sip::Header::To(_)
                | rsipstack::sip::Header::UserAgent(_)
                | rsipstack::sip::Header::Allow(_)
        );
        if !skip {
            headers.insert(h.name().to_string(), h.value().to_string());
        }
    }
    headers
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
            sip_headers: HashMap::new(),
            route_name: None,
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
        let ctx = ApplicationContext::new(db, make_call_info(), Arc::new(Config::default()));

        // Initially empty
        assert!(ctx.get_var("lang").await.is_none());

        // Set and get
        ctx.set_var("lang", "zh").await;
        assert_eq!(ctx.get_var("lang").await, Some("zh".to_string()));

        // Overwrite
        ctx.set_var("lang", "en").await;
        assert_eq!(ctx.get_var("lang").await, Some("en".to_string()));
    }

    #[test]
    fn test_routed_headers_override_originals_in_call_info() {
        // Simulate the merge logic used in SipSession::new():
        // routed headers should override original SIP request headers
        use rsipstack::sip::{Header, Request, Method, Uri};

        let mut req = Request {
            method: Method::Invite,
            uri: Uri::try_from("sip:test@pbx.com").unwrap(),
            version: rsipstack::sip::Version::V2,
            headers: vec![
                Header::Other("X-Custom".to_string(), "original-value".to_string()),
                Header::Other("X-Forwarded-For".to_string(), "192.168.1.1".to_string()),
            ].into(),
            body: vec![],
        };
        // Add a typed header to ensure it's still skipped by extract
        req.headers.push(
            rsipstack::sip::typed::From {
                display_name: None,
                uri: Uri::try_from("sip:alice@example.com").unwrap(),
                params: vec![],
            }.into(),
        );

        let original = extract_sip_headers(&req);
        assert_eq!(original.get("X-Custom").unwrap(), "original-value");
        assert_eq!(original.get("X-Forwarded-For").unwrap(), "192.168.1.1");
        assert!(original.get("From").is_none(), "From header should be skipped");

        // Simulate routing-modified headers (overriding X-Custom, adding P-Asserted-Identity)
        let routed_headers: Option<Vec<Header>> = Some(vec![
            Header::Other("X-Custom".to_string(), "routing-value".to_string()),
            Header::Other("P-Asserted-Identity".to_string(), "<sip:routing@pbx.com>".to_string()),
        ]);

        // Apply the same merge logic as in sip_session.rs
        let mut merged = original;
        if let Some(ref routed) = routed_headers {
            for h in routed {
                merged.insert(h.name().to_string(), h.value().to_string());
            }
        }

        // Verify routing headers override originals
        assert_eq!(
            merged.get("X-Custom").unwrap(),
            "routing-value",
            "routed headers should override original"
        );
        // Verify unmodified original headers are preserved
        assert_eq!(
            merged.get("X-Forwarded-For").unwrap(),
            "192.168.1.1",
            "unmodified original headers should persist"
        );
        // Verify new headers from routing are added
        assert_eq!(
            merged.get("P-Asserted-Identity").unwrap(),
            "<sip:routing@pbx.com>",
            "new routing headers should be present"
        );
    }
}
