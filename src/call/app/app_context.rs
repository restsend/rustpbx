use crate::call::app::ivr::trace::IvrTraceCollector;
use crate::config::Config;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use parking_lot::Mutex;
use sea_orm::{DatabaseConnection, DatabaseConnectionType};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppRouteContext {
    pub callee: String,
    pub sip_headers: HashMap<String, String>,
    pub variables: HashMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppInvocationContext {
    pub app_execution_id: u64,
    pub callee: String,
    pub sip_headers: HashMap<String, String>,
    pub variables: HashMap<String, String>,
}

pub struct AppSharedState {
    /// Arbitrary typed data, keyed by string.
    ///
    /// Use this to share state between addons (e.g., conference rooms, queue stats).
    pub custom_data: Arc<DashMap<String, Box<dyn std::any::Any + Send + Sync>>>,
}

impl AppSharedState {
    pub fn new() -> Self {
        Self {
            custom_data: Arc::new(DashMap::new()),
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
    pub session_vars: Arc<DashMap<String, String>>,

    /// Database connection (SeaORM).
    pub db: DatabaseConnection,

    /// HTTP client for outbound requests.
    pub http_client: reqwest::Client,

    /// Call metadata.
    pub call_info: CallInfo,

    /// Immutable metadata owned by the current application generation.
    pub invocation: Option<AppInvocationContext>,

    /// System configuration.
    pub config: Arc<Config>,

    /// RWI gateway for emitting real-time events.
    pub rwi_gateway: Option<crate::rwi::RwiGatewayRef>,

    /// IVR step trace collector for debugging (optional).
    pub ivr_trace: Option<Arc<IvrTraceCollector>>,

    /// Shared per-session typed extensions bag — same underlying `Arc` as
    /// [`CallSessionContext::extensions`][crate::proxy::proxy_call::session_hooks::CallSessionContext::extensions].
    /// Allows [`CallApp`]s to pass typed data to [`CallSessionHook`]s
    /// (e.g. CSAT results → CDR).
    pub session_extensions: crate::proxy::proxy_call::session_hooks::SessionExtensions,

    /// Pending queue plan + resolved agent URIs, set by SipSession before
    /// starting the queue app. The queue app factory reads (and clears) this.
    pub pending_queue: Arc<Mutex<Option<PendingQueuePlan>>>,

    /// Factory for creating chained sub-apps from IVR `start_app` actions.
    pub app_factory: Option<Arc<dyn crate::call::runtime::AppFactory>>,
}

/// Per-call overflow overrides carried via queue transfer URI query params
/// (`overflow_group` / `overflow_after` / `overflow_wait` / `overflow_mode`).
///
/// Priority: URI params > ACD policy > skill-group `overflow_groups`.
/// Partial-override semantics: only fields present on the URI are applied,
/// the rest fall back to the registry-synthesized escalation plan.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueueOverflowOverrides {
    /// `overflow_group=` — overflow target skill groups (replaces steps).
    pub groups: Vec<String>,
    /// `overflow_after=` — escalation trigger threshold (seconds).
    pub threshold_secs: Option<u64>,
    /// `overflow_wait=` — queue max wait before fallback (seconds).
    pub max_wait_secs: Option<u64>,
    /// `overflow_mode=` — `replace` or `cumulative`.
    pub mode: Option<crate::call::app::queue::EscalationMode>,
}

impl QueueOverflowOverrides {
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
            && self.threshold_secs.is_none()
            && self.max_wait_secs.is_none()
            && self.mode.is_none()
    }

    /// Parse an `overflow_mode=` value (`replace` / `cumulative`).
    pub fn parse_escalation_mode(s: &str) -> Option<crate::call::app::queue::EscalationMode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "replace" => Some(crate::call::app::queue::EscalationMode::Replace),
            "cumulative" => Some(crate::call::app::queue::EscalationMode::Cumulative),
            _ => None,
        }
    }

    /// Apply overrides onto a registry-synthesized escalation plan
    /// (partial-override semantics; empty overrides leave the plan untouched).
    pub fn apply_to_plan(&self, plan: &mut crate::call::app::queue::EscalationPlan) {
        use crate::call::app::queue::EscalationStep;
        if !self.groups.is_empty() {
            let threshold = self
                .threshold_secs
                .or_else(|| plan.steps.first().map(|s| s.threshold_secs))
                .unwrap_or(30);
            plan.steps = self
                .groups
                .iter()
                .map(|g| EscalationStep {
                    threshold_secs: threshold,
                    add_skill_group: g.clone(),
                    fair: true,
                })
                .collect();
        } else if let Some(t) = self.threshold_secs {
            for step in plan.steps.iter_mut() {
                step.threshold_secs = t;
            }
        }
        if let Some(m) = self.mode.clone() {
            plan.mode = m;
        }
    }
}

/// A resolved queue plan ready to be handed to QueueApp.
#[derive(Clone)]
pub struct PendingQueuePlan {
    pub plan: crate::call::QueuePlan,
    pub agent_uris: Vec<String>,
    pub parallel: bool,
    /// Primary skill-group id when the queue's dial target was
    /// `skill-group:{id}`. The queue app factory uses it to pull the
    /// escalation plan from the agent registry.
    pub skill_group_id: Option<String>,
    /// Per-call overflow overrides from the transfer URI query string.
    /// Applied by the queue app factory on top of the registry plan.
    pub overflow_overrides: Option<QueueOverflowOverrides>,
    /// `queue_joined` was already broadcast by `SipSession::start_queue_app`
    /// before agent resolution — strict ordering requires it to be the FIRST
    /// queue event. The queue app factory forwards this to `QueueApp` so
    /// `on_enter` does not emit a duplicate.
    pub joined_emitted: bool,
}

impl ApplicationContext {
    /// Create a new application context.
    pub fn new(
        db: DatabaseConnection,
        call_info: CallInfo,
        config: Arc<Config>,
        http_client: reqwest::Client,
    ) -> Self {
        Self {
            session_vars: Arc::new(DashMap::new()),
            db,
            http_client,
            call_info,
            invocation: None,
            config,
            rwi_gateway: None,
            ivr_trace: None,
            session_extensions: crate::proxy::proxy_call::session_hooks::SessionExtensions::new(),
            pending_queue: Arc::new(Mutex::new(None)),
            app_factory: None,
        }
    }

    /// Set a session variable.
    pub fn set_var(&self, key: impl Into<String>, value: impl Into<String>) {
        self.session_vars.insert(key.into(), value.into());
    }

    /// Get a session variable.
    pub fn get_var(&self, key: &str) -> Option<String> {
        self.session_vars.get(key).map(|r| r.value().clone())
    }

    /// Get a usable database connection reference, or None if disconnected.
    pub fn db_connection(&self) -> Option<&DatabaseConnection> {
        if matches!(&self.db.inner, DatabaseConnectionType::Disconnected) {
            None
        } else {
            Some(&self.db)
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

/// Merge route-produced headers into an existing snapshot using SIP's
/// case-insensitive header-name semantics.
pub fn merge_sip_headers(
    base: &HashMap<String, String>,
    routed: &[rsipstack::sip::Header],
) -> HashMap<String, String> {
    let mut merged = base.clone();
    for header in routed {
        let name = header.name().to_string();
        merged.retain(|key, _| !key.eq_ignore_ascii_case(&name));
        merged.insert(name, header.value().to_string());
    }
    merged
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
    fn test_overflow_overrides_apply_to_plan() {
        use crate::call::app::queue::{EscalationMode, EscalationPlan, EscalationStep};

        // Full override: groups replace steps, threshold + mode applied.
        let mut plan = EscalationPlan {
            mode: EscalationMode::Replace,
            steps: vec![EscalationStep {
                threshold_secs: 90,
                add_skill_group: "old".into(),
                fair: false,
            }],
        };
        QueueOverflowOverrides {
            groups: vec!["l2".into(), "l3".into()],
            threshold_secs: Some(30),
            max_wait_secs: None,
            mode: Some(EscalationMode::Cumulative),
        }
        .apply_to_plan(&mut plan);
        assert_eq!(plan.mode, EscalationMode::Cumulative);
        assert_eq!(plan.steps.len(), 2);
        assert!(plan.steps.iter().all(|s| s.threshold_secs == 30 && s.fair));
        assert_eq!(plan.steps[0].add_skill_group, "l2");
        assert_eq!(plan.steps[1].add_skill_group, "l3");

        // Partial override: only the threshold is rewritten.
        let mut plan2 = EscalationPlan {
            mode: EscalationMode::Cumulative,
            steps: vec![EscalationStep {
                threshold_secs: 90,
                add_skill_group: "a".into(),
                fair: true,
            }],
        };
        QueueOverflowOverrides {
            threshold_secs: Some(15),
            ..Default::default()
        }
        .apply_to_plan(&mut plan2);
        assert_eq!(plan2.steps[0].threshold_secs, 15);
        assert_eq!(plan2.steps[0].add_skill_group, "a");
        assert_eq!(plan2.mode, EscalationMode::Cumulative);

        // Groups without threshold: falls back to the first step's threshold.
        let mut plan4 = EscalationPlan {
            mode: EscalationMode::Replace,
            steps: vec![EscalationStep {
                threshold_secs: 60,
                add_skill_group: "old".into(),
                fair: false,
            }],
        };
        QueueOverflowOverrides {
            groups: vec!["l2".into()],
            ..Default::default()
        }
        .apply_to_plan(&mut plan4);
        assert_eq!(plan4.steps.len(), 1);
        assert_eq!(plan4.steps[0].threshold_secs, 60);
        assert_eq!(plan4.steps[0].add_skill_group, "l2");

        // Empty overrides leave the plan untouched.
        let mut plan3 = plan2.clone();
        let untouched = plan2.clone();
        QueueOverflowOverrides::default().apply_to_plan(&mut plan3);
        assert_eq!(plan3, untouched);
    }

    #[test]
    fn test_overflow_overrides_parse_escalation_mode() {
        assert_eq!(
            QueueOverflowOverrides::parse_escalation_mode("cumulative"),
            Some(crate::call::app::queue::EscalationMode::Cumulative)
        );
        assert_eq!(
            QueueOverflowOverrides::parse_escalation_mode(" Replace "),
            Some(crate::call::app::queue::EscalationMode::Replace)
        );
        assert_eq!(QueueOverflowOverrides::parse_escalation_mode("bogus"), None);
    }

    #[test]
    fn test_overflow_overrides_is_empty() {
        assert!(QueueOverflowOverrides::default().is_empty());
        assert!(
            !QueueOverflowOverrides {
                groups: vec!["g".into()],
                ..Default::default()
            }
            .is_empty()
        );
        assert!(
            !QueueOverflowOverrides {
                threshold_secs: Some(1),
                ..Default::default()
            }
            .is_empty()
        );
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
        let ctx = ApplicationContext::new(
            db,
            make_call_info(),
            Arc::new(Config::default()),
            reqwest::Client::new(),
        );

        // Initially empty
        assert!(ctx.get_var("lang").is_none());

        // Set and get
        ctx.set_var("lang", "zh");
        assert_eq!(ctx.get_var("lang"), Some("zh".to_string()));

        // Overwrite
        ctx.set_var("lang", "en");
        assert_eq!(ctx.get_var("lang"), Some("en".to_string()));
    }

    #[test]
    fn test_routed_headers_override_originals_in_call_info() {
        // Simulate the merge logic used in SipSession::new():
        // routed headers should override original SIP request headers
        use rsipstack::sip::{Header, Method, Request, Uri};

        let mut req = Request {
            method: Method::Invite,
            uri: Uri::try_from("sip:test@pbx.com").unwrap(),
            version: rsipstack::sip::Version::V2,
            headers: vec![
                Header::Other("X-Custom".to_string(), "original-value".to_string()),
                Header::Other("x-custom".to_string(), "duplicate-value".to_string()),
                Header::Other("X-Forwarded-For".to_string(), "192.168.1.1".to_string()),
            ]
            .into(),
            body: vec![],
        };
        // Add a typed header to ensure it's still skipped by extract
        req.headers.push(
            rsipstack::sip::typed::From {
                display_name: None,
                uri: Uri::try_from("sip:alice@example.com").unwrap(),
                params: vec![],
            }
            .into(),
        );

        let original = extract_sip_headers(&req);
        assert_eq!(original.get("X-Custom").unwrap(), "original-value");
        assert_eq!(original.get("x-custom").unwrap(), "duplicate-value");
        assert_eq!(original.get("X-Forwarded-For").unwrap(), "192.168.1.1");
        assert!(
            original.get("From").is_none(),
            "From header should be skipped"
        );

        // Simulate routing-modified headers (overriding X-Custom, adding P-Asserted-Identity)
        let routed_headers: Option<Vec<Header>> = Some(vec![
            Header::Other("x-custom".to_string(), "routing-value".to_string()),
            Header::Other(
                "P-Asserted-Identity".to_string(),
                "<sip:routing@pbx.com>".to_string(),
            ),
        ]);

        // Apply the same merge logic as in sip_session.rs
        let merged = merge_sip_headers(&original, routed_headers.as_deref().unwrap_or_default());

        // Verify routing headers override originals
        assert_eq!(
            merged.get("x-custom").unwrap(),
            "routing-value",
            "routed headers should override original"
        );
        assert_eq!(
            merged
                .keys()
                .filter(|key| key.eq_ignore_ascii_case("X-Custom"))
                .count(),
            1
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
