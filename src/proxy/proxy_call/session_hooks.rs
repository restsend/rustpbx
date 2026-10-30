//! Call-session lifecycle hooks.
//!
//! Any component that needs to react to call events (connected, held, ended, …)
//! can implement [`CallSessionHook`] and register it via
//! [`crate::proxy::server::SipServerBuilder::with_session_hook`].
//!
//! All callback methods have default no-op implementations so implementors
//! only override what they care about.

use crate::callrecord::CallRecordHangupReason;
use async_trait::async_trait;
use std::fmt;
use std::sync::Arc;

/// Forward-declare AppRuntime so the hook method can accept it without
/// creating a circular dependency at the module level.
use crate::call::runtime::AppRuntime;

/// Per-session typed extensions bag (like [`http::Extensions`]) wrapped in an
/// [`Arc`] so it can be cheaply cloned (all clones share the same bag).
/// Automatically cleaned up when the last reference is dropped (session ends).
///
/// Addons use this to pass typed data across [`CallSessionHook`] callbacks
/// without resorting to a global key-value store.
#[derive(Clone)]
pub struct SessionExtensions(pub Arc<parking_lot::RwLock<http::Extensions>>);

impl SessionExtensions {
    /// Create a new empty session extensions bag.
    pub fn new() -> Self {
        Self(Arc::new(parking_lot::RwLock::new(http::Extensions::new())))
    }

    /// Extension key of the session-level pinned agent id: planted at target
    /// resolution (queue/skill-group resolve, RWI originate) and updated when
    /// the connected leg is a validated agent. Shared write/read contract
    /// between core session flows and the CC addon.
    pub fn agent_attribution(&self) -> Option<String> {
        self.read()
            .get::<std::collections::HashMap<String, String>>()
            .and_then(|m| m.get(Self::AGENT_ATTRIBUTION_KEY).cloned())
            .filter(|s| !s.is_empty())
    }

    /// Whether an agent attribution has been planted.
    pub fn has_agent_attribution(&self) -> bool {
        self.read()
            .get::<std::collections::HashMap<String, String>>()
            .is_some_and(|m| m.contains_key(Self::AGENT_ATTRIBUTION_KEY))
    }

    /// Plant/update the pinned agent id.
    pub fn set_agent_attribution(&self, agent_id: &str) {
        let mut ext = self.write();
        if ext.get_mut::<std::collections::HashMap<String, String>>().is_none() {
            ext.insert(std::collections::HashMap::<String, String>::new());
        }
        ext.get_mut::<std::collections::HashMap<String, String>>()
            .expect("map inserted above")
            .insert(Self::AGENT_ATTRIBUTION_KEY.to_string(), agent_id.to_string());
    }
}

/// Storage key backing [`SessionExtensions::agent_attribution`].
impl SessionExtensions {
    pub const AGENT_ATTRIBUTION_KEY: &'static str = "resolved_agent_id";
}

/// A typed, addon-contributed RWI event enrichment section (payload part —
/// dyn-compatible). Implemented together with [`EventSectionSpec`], which
/// carries the static wire key.
pub trait EventSection: Send + Sync + 'static {
    /// Serialize this section's payload (the object nested under `KEY`).
    fn payload(&self) -> serde_json::Result<serde_json::Value>;
}

/// Static wire key for an [`EventSection`] — a separate trait so sections
/// stay `dyn`-storeable while the KEY remains a compile-time constant
/// resolved at the typed insertion point.
pub trait EventSectionSpec: EventSection {
    /// Top-level wire key for this section. Contract: snake_case, globally
    /// unique across all addons, and must not collide with the reserved
    /// context/payload keys ([`SessionExtensions::RESERVED_EVENT_KEYS`] —
    /// asserted on insert).
    const KEY: &'static str;
}

/// Container for all typed event sections on one extensions bag. Stored as
/// a single well-known typed entry so `http::Extensions` type-keying keeps
/// the addon API fully typed (`set_event_section::<MySection>`); the KEY →
/// section map lives behind an Arc because this `http` version's
/// `Extensions::insert` is `Clone`-bounded — replacing the entry clones the
/// handle, never the sections. All access happens under the bag's RwLock.
#[derive(Clone, Default)]
struct EventSections {
    map: std::sync::Arc<
        parking_lot::RwLock<std::collections::HashMap<&'static str, Box<dyn EventSection>>>,
    >,
}

/// Top-level payload/context keys an [`EventSectionSpec::KEY`] must never shadow
/// (flat context fields + gateway-injected fields + core envelope fields).
pub const RESERVED_EVENT_KEYS: &[&str] = &[
    "event_type",
    "call_id",
    "leg_id",
    "leg_role",
    "session_id",
    "caller",
    "callee",
    "caller_name",
    "callee_name",
    "direction",
    "trunk",
    "app_id",
    "routing_target",
    "agent_id",
    "agent_name",
    "queue_id",
    "root",
    "user_data",
    "node_ip",
    "client_ip",
    "recording_id",
    "transfer_source",
    "transfer_target",
];

impl SessionExtensions {
    /// Insert/replace a typed event section (addon API). The section rides
    /// every subsequent call-scoped event of this session under its KEY.
    ///
    /// # Panics (debug builds)
    /// Panics when `S::KEY` collides with a reserved context/payload key —
    /// a programmer error caught at development time (uniqueness across
    /// addons is each addon's own contract, covered by tests).
    pub fn set_event_section<S: EventSectionSpec>(&self, section: S) {
        debug_assert!(
            !RESERVED_EVENT_KEYS.contains(&S::KEY),
            "EventSection KEY {:?} collides with a reserved payload key",
            S::KEY
        );
        let mut ext = self.write();
        if ext.get_mut::<EventSections>().is_none() {
            ext.insert(EventSections::default());
        }
        let sections = ext
            .get_mut::<EventSections>()
            .expect("EventSections just inserted");
        sections.map.write().insert(S::KEY, Box::new(section));
    }

    /// Remove a typed event section by key (addon API).
    pub fn remove_event_section(&self, key: &'static str) {
        let mut ext = self.write();
        if let Some(sections) = ext.get_mut::<EventSections>() {
            sections.map.write().remove(key);
        }
    }

    /// Materialize all sections into `(KEY, JSON)` pairs (emission choke
    /// point). Sections whose payload fails to serialize are skipped with a
    /// warning — a broken addon section must never break event emission.
    pub fn event_sections(&self) -> Vec<(&'static str, serde_json::Value)> {
        let ext = self.read();
        let Some(sections) = ext.get::<EventSections>() else {
            return Vec::new();
        };
        sections
            .map
            .read()
            .iter()
            .filter_map(|(key, section)| match section.payload() {
                Ok(value) => Some((*key, value)),
                Err(e) => {
                    tracing::warn!(section = key, error = %e, "event section serialization failed");
                    None
                }
            })
            .collect()
    }
}

impl Default for SessionExtensions {
    fn default() -> Self {
        Self::new()
    }
}

impl std::ops::Deref for SessionExtensions {
    type Target = Arc<parking_lot::RwLock<http::Extensions>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Lightweight context passed to every session-hook callback.
/// All fields are cheaply cloneable.
pub struct CallSessionContext {
    /// Unique session identifier (UUID string).
    pub session_id: String,
    /// Immutable lifetime of the leg that produced this callback, when known.
    pub leg_generation: Option<uuid::Uuid>,
    /// Root session id of the whole logical call (first INVITE Call-ID,
    /// inherited across transfers). `None` means this session is the root —
    /// resolve with `session_id`. See `call::uui` / RFC 7433 UUI.
    pub root_session_id: Option<String>,
    /// Original caller URI / number as it arrived.
    pub caller: String,
    /// Original callee URI / number as it arrived.
    pub callee: String,
    /// The callee URI that was actually connected (set after answer).
    pub connected_callee: Option<String>,
    /// Queue name if the call was routed through a queue.
    pub queue_name: Option<String>,
    /// Primary skill-group id when the queue dials a `skill-group:{id}`
    /// target. Skill-group configuration lookups (CSAT survey, wrapup,
    /// hold-music) must use this — the queue name is an unrelated label.
    pub skill_group_id: Option<String>,
    /// True when this logical call went through a transfer (inbound REFER
    /// blind transfer, consultative transfer / 3-way merge) before reaching
    /// the current leg. Post-call hooks suppress CSAT surveys on it.
    pub transferred: bool,
    /// Dial intent the *caller's own device* declared on the initial INVITE
    /// (RFC 7433 `User-to-User` `leg=` parameter — `consult` / `transfer`,
    /// see `call::uui`). Persisted into `cc_calls.leg_role` by the CC hook
    /// so agent today-metrics can keep these self-dialed sub-call legs out
    /// of the counters; it never shapes the call lifecycle.
    pub leg_role: Option<String>,
    /// Call direction: `"inbound"` or `"outbound"`.
    pub direction: String,
    /// ISO-8601 timestamp when the session was created (before ringing).
    pub started_at: Option<String>,
    /// Per-session typed extensions bag (session cookie). Any addon can
    /// insert/read typed data here (e.g. `ctx.extensions.write().insert(MyData)`).
    /// Clones share the same underlying bag. Automatically cleaned up when the
    /// session ends.
    ///
    /// Routing-layer metadata (X-CRM-* / X-CC-* headers) is pre-populated as
    /// `HashMap<String, String>` — read via
    /// `ctx.extensions.read().get::<HashMap<String, String>>()`.
    pub extensions: SessionExtensions,
}

impl CallSessionContext {
    /// Bind a callback to its producing leg without changing shared metadata.
    pub fn with_leg(mut self, leg: &crate::call::domain::Leg) -> Self {
        self.leg_generation = Some(leg.lifecycle_id);
        self
    }

    /// Logical-call correlation id: root session id when this leg is a child,
    /// otherwise this leg's own `session_id`.
    pub fn logical_session_id(&self) -> &str {
        self.root_session_id
            .as_deref()
            .unwrap_or(self.session_id.as_str())
    }
}

impl Clone for CallSessionContext {
    fn clone(&self) -> Self {
        Self {
            session_id: self.session_id.clone(),
            leg_generation: self.leg_generation,
            root_session_id: self.root_session_id.clone(),
            caller: self.caller.clone(),
            callee: self.callee.clone(),
            connected_callee: self.connected_callee.clone(),
            queue_name: self.queue_name.clone(),
            skill_group_id: self.skill_group_id.clone(),
            transferred: self.transferred,
            leg_role: self.leg_role.clone(),
            direction: self.direction.clone(),
            started_at: self.started_at.clone(),
            extensions: self.extensions.clone(),
        }
    }
}

impl fmt::Debug for CallSessionContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallSessionContext")
            .field("session_id", &self.session_id)
            .field("leg_generation", &self.leg_generation)
            .field("root_session_id", &self.root_session_id)
            .field("caller", &self.caller)
            .field("callee", &self.callee)
            .field("connected_callee", &self.connected_callee)
            .field("queue_name", &self.queue_name)
            .field("skill_group_id", &self.skill_group_id)
            .field("transferred", &self.transferred)
            .field("leg_role", &self.leg_role)
            .field("direction", &self.direction)
            .field("started_at", &self.started_at)
            .field("extensions", &"<http::Extensions>")
            .finish()
    }
}

/// Completion intent returned by [`CallSessionHook::on_app_exited`].
/// The session uses this to decide what post-exit actions to take.
pub struct IvrExecCompletion {
    /// If set, the session will unhold this leg (restore to sendrecv).
    pub unhold_leg: Option<LegId>,
    /// Policy resources acquired while resolving the application route.
    pub route_hints: Option<crate::config::DialplanHints>,
    /// If set, the session will send a SIP INFO to this leg with the given
    /// content-type and body.
    pub result_info: Option<SendInfoSpec>,
}

/// Describes a SIP INFO request to be sent to a specific dialog.
pub struct SendInfoSpec {
    pub leg_id: LegId,
    pub content_type: String,
    pub body: Vec<u8>,
}

use crate::call::domain::LegId;

/// Observer trait for call-session lifecycle events.
///
/// Implementations must be `Send + Sync` so they can be stored in a shared
/// `Arc` and called from async tasks.
#[async_trait]
pub trait CallSessionHook: Send + Sync {
    /// Called when the callee starts ringing — any ringing-ish provisional
    /// response: 180 Ringing, or 183/180-with-SDP early media.
    ///
    /// `early_media` is `true` when the provisional response carried SDP
    /// (e.g. 183 Session Progress with early media) and `false` for a plain
    /// 180 Ringing without SDP.
    async fn on_call_ringing(&self, _ctx: &CallSessionContext, _early_media: bool) {}

    /// Called once both legs are connected (200 OK acknowledged by caller).
    async fn on_call_connected(&self, _ctx: &CallSessionContext) {}

    /// A leg rejected, timed out, or disconnected. Implementations may
    /// update their state and return a SIP INFO notification.
    async fn on_leg_failed(
        &self,
        _ctx: &CallSessionContext,
        _leg_id: &str,
        _reason: &str,
    ) -> Option<SendInfoSpec> {
        None
    }

    /// Called when a leg is put on hold (re-INVITE with `sendonly`/`inactive`).
    async fn on_call_held(&self, _ctx: &CallSessionContext, _leg_id: &str) {}

    /// Called when a previously held leg is retrieved.
    async fn on_call_unheld(&self, _ctx: &CallSessionContext, _leg_id: &str) {}

    /// Called at the very end of a session (during cleanup), regardless of how
    /// the call ended.  `duration_secs` is 0 for unanswered calls.
    async fn on_call_ended(
        &self,
        _ctx: &CallSessionContext,
        _reason: Option<&CallRecordHangupReason>,
        _duration_secs: u64,
    ) {
    }

    /// Called when the connected callee (agent) terminates while the caller
    /// is still connected.  Return `true` to suppress normal hangup — the
    /// hook takes over the caller session (e.g. starting a survey app).
    ///
    /// Fires *before* [`on_call_ended`] so the hook can transition the
    /// session to a new app before final cleanup.
    async fn on_agent_disconnected(
        &self,
        _ctx: &CallSessionContext,
        _app_runtime: &dyn AppRuntime,
    ) -> bool {
        false
    }

    /// Called after an application exits (CallApp event loop finished).
    /// Return an [`IvrExecCompletion`] to instruct the session to perform
    /// post-exit actions (unhold, send INFO result, etc.).
    ///
    /// The default implementation returns `None` (no action).
    async fn on_app_exited(
        &self,
        _ctx: &CallSessionContext,
        _app_execution_id: Option<u64>,
    ) -> Option<IvrExecCompletion> {
        None
    }
}


#[cfg(test)]
mod event_section_tests {
    use super::*;
    use serde_json::json;

    struct SampleSection {
        skill: String,
    }

    impl EventSection for SampleSection {
        fn payload(&self) -> serde_json::Result<serde_json::Value> {
            Ok(json!({"skill": self.skill}))
        }
    }

    impl EventSectionSpec for SampleSection {
        const KEY: &'static str = "ccsample";
    }

    /// Set → materialize → replace contract on the typed bag.
    #[test]
    fn sections_set_replace_materialize() {
        let ext = SessionExtensions::new();
        assert!(ext.event_sections().is_empty(), "no sections initially");

        ext.set_event_section(SampleSection { skill: "l1".into() });
        let sections = ext.event_sections();
        assert_eq!(sections, vec![("ccsample", json!({"skill": "l1"}))]);

        // Replace: same KEY, new payload.
        ext.set_event_section(SampleSection { skill: "l2".into() });
        assert_eq!(ext.event_sections(), vec![("ccsample", json!({"skill": "l2"}))]);

        ext.remove_event_section("ccsample");
        assert!(ext.event_sections().is_empty());
    }

    /// A section KEY colliding with a reserved payload key is a programmer
    /// error caught at development time.
    #[test]
    #[should_panic(expected = "collides with a reserved payload key")]
    fn reserved_key_collision_panics_in_debug() {
        struct Bad;
        impl EventSection for Bad {
            fn payload(&self) -> serde_json::Result<serde_json::Value> {
                Ok(json!({}))
            }
        }
        impl EventSectionSpec for Bad {
            const KEY: &'static str = "agent_id";
        }
        let ext = SessionExtensions::new();
        ext.set_event_section(Bad);
    }

    /// Sections survive the bag clones (Arc-shared), mirroring how the
    /// session hands the bag to hook contexts.
    #[test]
    fn sections_shared_across_bag_clones() {
        let ext = SessionExtensions::new();
        ext.set_event_section(SampleSection { skill: "l1".into() });
        let clone = ext.clone();
        assert_eq!(clone.event_sections(), ext.event_sections());
    }
}
