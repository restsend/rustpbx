//! Standardized call-error registry.
//!
//! Every call failure that can surface in a call record is described by a
//! stable, enumerated [`CallErrInfo`] entry grouped by the subsystem (`app`)
//! that produced it.  Catalogs are `const` slices owned by each subsystem and
//! merged at startup into a [`CallErrRegistry`] that powers:
//!
//! * call-record error rendering (the `error_code` metadata key is resolved
//!   back to a localized message + remediation hint), and
//! * the live operation-manual page (`/console/error-codes`) which renders the
//!   merged catalog straight from code so it can never drift.
//!
//! The `code` is hierarchical (`"<app>.<snake>"`, e.g.
//! `wholesale.insufficient_funds`) and is the single stable identifier stored
//! in the call record.  Free-text dynamic detail (e.g. `"limit: 100"`) is
//! carried separately alongside the code so the code remains queryable.

use crate::callrecord::CallRecordHangupReason;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

/// Operational severity of an error, used for UI colouring and filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrSeverity {
    /// Informational / expected outcome (e.g. caller cancelled).
    Info,
    /// Recoverable or policy-driven rejection worth attention.
    Warn,
    /// Hard failure requiring operator action.
    Error,
}

impl ErrSeverity {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrSeverity::Info => "info",
            ErrSeverity::Warn => "warn",
            ErrSeverity::Error => "error",
        }
    }
}

/// A single standardized error definition.
///
/// Entries live in `const` slices (see [`CallErrCatalog`]) and are referenced
/// by `&'static CallErrInfo` throughout the call pipeline.
#[derive(Debug, Clone)]
pub struct CallErrInfo {
    /// Subsystem that owns this error: `wholesale`, `proxy`, `acl`,
    /// `http_router`, `ivr`, `voicemail`, `queue`, `dial`, `transfer`.
    pub app: &'static str,
    /// Stable hierarchical code, e.g. `wholesale.insufficient_funds`.
    /// Must start with `<app>.`.
    pub code: &'static str,
    /// Default English message (the generic form, without runtime variables).
    /// Runtime detail is carried separately.
    pub message: &'static str,
    /// Canonical SIP response code for this error, if applicable.
    pub sip_status: Option<u16>,
    /// The "who ended it / outcome" dimension. Reuses the existing call-record
    /// taxonomy so the registry is the single source for the cause→outcome map.
    pub hangup_reason: CallRecordHangupReason,
    /// Severity for UI colouring and manual grouping.
    pub severity: ErrSeverity,
    /// i18n key resolving to the localized message, e.g.
    /// `errors.wholesale.insufficient_funds`.
    pub locale_key: &'static str,
    /// Optional i18n key for the remediation / operator-handling hint.
    pub remediation_key: Option<&'static str>,
}

impl CallErrInfo {
    /// Convenience accessor for catalogs that want to reference an entry by the
    /// const slice + index without repeating the code string.
    pub const fn from_slice(catalog: &'static [CallErrInfo], idx: usize) -> &'static CallErrInfo {
        &catalog[idx]
    }
}

/// Implemented by every subsystem that owns a `const` catalog of errors.
///
/// Implementations are aggregated at startup by [`CallErrRegistry::merge`].
/// Each addon gates its implementation behind its cargo feature so the merged
/// registry only contains entries for compiled-in subsystems.
pub trait CallErrCatalog: Send + Sync {
    /// The subsystem's static catalog of error definitions.
    fn catalog() -> &'static [CallErrInfo];
}

/// Merged, read-only view over all registered error catalogs.
///
/// Built once at startup ([`CallErrRegistry::build`]) and shared (via
/// [`Arc`]) with HTTP handlers that need to resolve a code for display or
/// enumerate the full catalog for the operations manual.
#[derive(Debug, Default, Clone)]
pub struct CallErrRegistry {
    /// All entries, sorted by (`app`, `code`) for stable manual rendering.
    entries: Vec<&'static CallErrInfo>,
    /// Fast lookup by stable code string.
    by_code: HashMap<&'static str, &'static CallErrInfo>,
}

impl CallErrRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Merge a single catalog into the registry. Idempotent on re-registration:
    /// a duplicate `code` replaces the prior entry (keeps the registry
    /// consistent if an addon is re-registered).
    pub fn merge<C: CallErrCatalog>(&mut self) {
        self.merge_slice(C::catalog());
    }

    /// Merge a raw `&'static [CallErrInfo]` slice.
    pub fn merge_slice(&mut self, catalog: &'static [CallErrInfo]) {
        for info in catalog {
            self.by_code.insert(info.code, info);
        }
        self.rebuild_entries();
    }

    fn rebuild_entries(&mut self) {
        let mut all: Vec<&'static CallErrInfo> = self.by_code.values().copied().collect();
        all.sort_by(|a, b| (a.app, a.code).cmp(&(b.app, b.code)));
        self.entries = all;
    }

    /// Resolve a code to its definition. Returns `None` for unknown codes
    /// (e.g. a code from a newer build persisted in an old record).
    pub fn find(&self, code: &str) -> Option<&'static CallErrInfo> {
        self.by_code.get(code).copied()
    }

    /// All registered entries, sorted by (`app`, `code`).
    pub fn all(&self) -> &[&'static CallErrInfo] {
        &self.entries
    }

    /// Number of registered entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the registry holds no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Build the merged registry from all compiled-in catalogs. Called once at
/// startup. Each addon catalog is gated by its cargo feature.
/// Build a core-only registry (no addon catalogs). App startup merges addons
/// via [`AddonRegistry::merge_error_catalogs_into`] then [`install_registry`].
pub fn build_core_registry() -> CallErrRegistry {
    build_registry_inner()
}

/// Build the merged registry from core catalogs only, wrapped in Arc.
/// Prefer app-startup merge + [`install_registry`] when addons are available.
pub fn build_registry() -> Arc<CallErrRegistry> {
    Arc::new(build_registry_inner())
}

fn build_registry_inner() -> CallErrRegistry {
    let mut reg = CallErrRegistry::new();

    // Core (non-addon) catalogs — always present.
    reg.merge_slice(crate::proxy::error_catalog::CATALOG);
    reg.merge_slice(crate::proxy::routing::error_catalog::CATALOG);
    reg.merge_slice(crate::proxy::routing::http_error_catalog::CATALOG);
    reg.merge_slice(crate::proxy::proxy_call::error_catalog::CATALOG);
    reg.merge_slice(crate::call::app::error_catalog::CATALOG);

    // Compiled-in addon catalogs. AppState may still merge a live AddonRegistry
    // and install_registry() (first-wins); including features here keeps unit
    // tests and early registry() callers consistent with the binary features.
    #[cfg(feature = "addon-wholesale")]
    crate::addons::registry::merge_compiled_addon_error_catalogs(&mut reg);

    reg
}

/// Process-wide merged registry. Populated at app startup via
/// [`install_registry`]; falls back to core-only catalogs if never installed
/// (unit tests / early handlers).
static REGISTRY: OnceLock<CallErrRegistry> = OnceLock::new();

/// Install the process-wide registry (core + addon catalogs). Idempotent —
/// first call wins.
pub fn install_registry(reg: CallErrRegistry) {
    let _ = REGISTRY.set(reg);
}

/// Access the process-wide merged error registry.
pub fn registry() -> &'static CallErrRegistry {
    REGISTRY.get_or_init(build_registry_inner)
}

// ─────────────────────────────────────────────────────────────────────────────
// Call trace
// ─────────────────────────────────────────────────────────────────────────────
//
// A call trace is an ordered, persistent timeline of the call from the
// operator's perspective: ring → answer → IVR → queue → transfer → bridge →
// hold/resume → hangup, interleaved with `media.play` records (with duration
// and interruption) and error/warn/info outcomes. It is stored in the
// call-record `metadata` JSON under the `trace` key as a real JSON array, and
// complements (rather than replaces) the real-time RWI event stream.

/// High-level call-timeline event kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceKind {
    /// An INVITE was sent to a target / ringing started.
    Ring,
    /// The callee answered and the call was established.
    Answer,
    /// Caller↔callee legs were bridged (media connected).
    Bridge,
    /// A leg was placed on hold.
    Hold,
    /// A held leg was resumed.
    Resume,
    /// An IVR (or other application flow) started/ended.
    Ivr,
    /// A voicemail flow event (routed to mailbox, recording, replay, delete).
    Voicemail,
    /// Call entered a queue (or queue-related transition).
    Queue,
    /// A transfer was attempted (attended/blind) — success or failure.
    Transfer,
    /// An audio file / media playback started or finished.
    Play,
    /// The RTP-inactivity watchdog fired (no media from one side).
    RtpTimeout,
    /// The call ended (terminal event).
    End,
}

/// A single event in the call trace.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TraceEvent {
    /// Milliseconds offset from the start of the session.
    pub ts: i64,
    pub kind: TraceKind,
    /// Severity (error/warn/info). `None` for neutral transitions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<ErrSeverity>,
    /// Standardized registry code, e.g. `ivr.timeout`, `wholesale.insufficient_funds`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Human-readable message.
    pub message: String,
    /// Duration (ms) — e.g. how long a `media.play` ran, or queue wait time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    /// Whether a `Play` was interrupted before completing naturally.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interrupted: Option<bool>,
    /// Optional structured detail (file path, hangup initiator, agent id, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

impl TraceEvent {
    pub fn new(kind: TraceKind, message: impl Into<String>) -> Self {
        Self {
            ts: 0,
            kind,
            severity: None,
            code: None,
            message: message.into(),
            duration_ms: None,
            interrupted: None,
            detail: None,
        }
    }

    pub fn severity(mut self, severity: ErrSeverity) -> Self {
        self.severity = Some(severity);
        self
    }

    pub fn code(mut self, code: &str) -> Self {
        self.code = Some(code.to_string());
        self
    }

    pub fn duration(mut self, ms: i64) -> Self {
        self.duration_ms = Some(ms);
        self
    }

    pub fn interrupted(mut self, interrupted: bool) -> Self {
        self.interrupted = Some(interrupted);
        self
    }

    pub fn detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Some(detail);
        self
    }
}

/// Append a trace event to a call record's `metadata["trace"]` JSON array.
/// Called by core (reporter) and by addons in `CallRecordHook::on_record_enrich`
/// so every subsystem can contribute to the diagnostic timeline. Creates the
/// `trace` array on first use and preserves existing events.
pub fn append_trace(
    metadata: &mut std::collections::HashMap<String, serde_json::Value>,
    event: TraceEvent,
) {
    let arr = metadata
        .entry("trace".to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    if let serde_json::Value::Array(items) = arr {
        if let Ok(v) = serde_json::to_value(event) {
            items.push(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callrecord::CallRecordHangupReason;

    const fn info(code: &'static str, app: &'static str) -> CallErrInfo {
        CallErrInfo {
            app,
            code,
            message: code,
            sip_status: Some(500),
            hangup_reason: CallRecordHangupReason::Failed,
            severity: ErrSeverity::Error,
            locale_key: code,
            remediation_key: None,
        }
    }

    static FIXTURES: &[CallErrInfo] = &[
        info("proxy.b", "proxy"),
        info("acl.a", "acl"),
        info("proxy.a", "proxy"),
    ];

    static OLD_X: &[CallErrInfo] = &[CallErrInfo {
        app: "proxy",
        code: "proxy.x",
        message: "old",
        sip_status: None,
        hangup_reason: CallRecordHangupReason::Failed,
        severity: ErrSeverity::Warn,
        locale_key: "proxy.x",
        remediation_key: None,
    }];

    static NEW_X: &[CallErrInfo] = &[CallErrInfo {
        app: "proxy",
        code: "proxy.x",
        message: "new",
        sip_status: None,
        hangup_reason: CallRecordHangupReason::Failed,
        severity: ErrSeverity::Error,
        locale_key: "proxy.x",
        remediation_key: None,
    }];

    #[test]
    fn registry_merge_find_sorted() {
        let mut reg = CallErrRegistry::new();
        reg.merge_slice(FIXTURES);
        // find works
        assert!(reg.find("proxy.a").is_some());
        assert!(reg.find("missing").is_none());
        // sorted by (app, code): acl.a, proxy.a, proxy.b
        let codes: Vec<&str> = reg.all().iter().map(|i| i.code).collect();
        assert_eq!(codes, vec!["acl.a", "proxy.a", "proxy.b"]);
    }

    #[test]
    fn registry_dedup_last_wins() {
        let mut reg = CallErrRegistry::new();
        reg.merge_slice(OLD_X);
        reg.merge_slice(NEW_X);
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.find("proxy.x").unwrap().message, "new");
    }

    #[test]
    fn catalog_entries_have_consistent_app_prefix() {
        // Every catalog code must start with "<app>.".
        let reg = crate::call_errors::build_registry();
        for entry in reg.all() {
            let prefix = format!("{}.", entry.app);
            assert!(
                entry.code.starts_with(&prefix),
                "code {} does not start with app prefix {}",
                entry.code,
                prefix
            );
            assert!(
                !entry.locale_key.is_empty(),
                "code {} missing locale_key",
                entry.code
            );
        }
    }

    #[test]
    fn append_trace_builds_and_preserves_array() {
        let mut metadata = std::collections::HashMap::new();
        append_trace(
            &mut metadata,
            TraceEvent::new(TraceKind::Ring, "Dialing").severity(ErrSeverity::Info),
        );
        append_trace(
            &mut metadata,
            TraceEvent::new(TraceKind::End, "Call ended").severity(ErrSeverity::Warn),
        );
        let arr = metadata
            .get("trace")
            .and_then(|v| v.as_array())
            .expect("trace is an array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["kind"], "ring");
        assert_eq!(arr[0]["severity"], "info");
        assert_eq!(arr[1]["kind"], "end");
        // A pre-existing trace array is appended to, not replaced.
        append_trace(&mut metadata, TraceEvent::new(TraceKind::Play, "Prompt"));
        assert_eq!(
            metadata
                .get("trace")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(3)
        );
    }

    #[test]
    fn trace_event_serializes_duration_and_interruption() {
        let ev = TraceEvent::new(TraceKind::Play, "Played prompt")
            .duration(1234)
            .interrupted(true);
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["kind"], "play");
        assert_eq!(v["duration_ms"], 1234);
        assert_eq!(v["interrupted"], true);
    }
}
