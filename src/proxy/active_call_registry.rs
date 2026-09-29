use crate::proxy::proxy_call::sip_session::SipSessionHandle;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tokio::sync::Notify;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize)]
pub enum ActiveProxyCallStatus {
    Ringing,
    Talking,
}

impl std::fmt::Display for ActiveProxyCallStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActiveProxyCallStatus::Ringing => write!(f, "ringing"),
            ActiveProxyCallStatus::Talking => write!(f, "talking"),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ActiveProxyCallEntry {
    pub session_id: String,
    pub caller: Option<String>,
    pub callee: Option<String>,
    pub direction: String,
    pub started_at: DateTime<Utc>,
    pub answered_at: Option<DateTime<Utc>>,
    pub status: ActiveProxyCallStatus,
}

/// CC / desk call context stored alongside the active session so
/// `GET /cc/calls/{call_id}/context` can return queue / skill / IVR / CRM
/// fields after the queue-location enricher has run (contract §3.1).
#[derive(Clone, Debug, Default, Serialize)]
pub struct ActiveCallContextMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_group_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ivr_node_id: Option<String>,
    /// Wall-clock time the call entered its current IVR app — stamped next
    /// to the `ivr` session ext when the app starts so the monitor can show
    /// how long the call has been sitting in the IVR.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ivr_entered_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub customer_id: Option<String>,
}

/// Wall-clock millis for the registry heartbeat (process-local).
fn heartbeat_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Snapshot of the per-session event-loop heartbeats (metrics sampler input).
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct HeartbeatStats {
    /// Session loops that have bumped a heartbeat.
    pub tracked: usize,
    /// Age of the oldest heartbeat, seconds.
    pub max_age_secs: u64,
    /// Heartbeats at or beyond the staleness threshold.
    pub stale: usize,
}

pub struct ActiveProxyCallRegistry {
    entries: DashMap<String, ActiveProxyCallEntry>,
    handles: DashMap<String, SipSessionHandle>,
    // Lookup keys are either a full Dialog-ID or a plain Call-ID alias;
    // session ownership is indexed by bare SIP Call-ID (dialog tags not part of the key).
    handles_by_dialog: DashMap<String, SipSessionHandle>,
    dialog_by_session: DashMap<String, Vec<String>>,
    context_meta: DashMap<String, ActiveCallContextMeta>,
    /// Per-session event-loop heartbeat (unix millis); staleness tells the
    /// CC stuck-binding watchdog a hung loop from an idle-but-alive one.
    heartbeats: DashMap<String, Arc<AtomicU64>>,
    change_notify: Notify,
}

impl Default for ActiveProxyCallRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ActiveProxyCallRegistry {
    pub fn new() -> Self {
        Self {
            entries: DashMap::new(),
            handles: DashMap::new(),
            handles_by_dialog: DashMap::new(),
            dialog_by_session: DashMap::new(),
            context_meta: DashMap::new(),
            heartbeats: DashMap::new(),
            change_notify: Notify::new(),
        }
    }

    /// Record a main-loop heartbeat for `session_id`.
    pub fn touch_heartbeat(&self, session_id: &str) {
        let now = heartbeat_now_ms();
        let slot = self
            .heartbeats
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(AtomicU64::new(now)))
            .clone();
        slot.store(now, std::sync::atomic::Ordering::Relaxed);
    }

    /// Heartbeat gauges for the metrics sampler (`stale` counts ages >=
    /// `stale_after_secs`). Sessions without a heartbeat are not tracked.
    pub fn heartbeat_stats(&self, stale_after_secs: u64) -> HeartbeatStats {
        let mut stats = HeartbeatStats::default();
        for entry in self.heartbeats.iter() {
            let age = heartbeat_now_ms()
                .saturating_sub(entry.value().load(std::sync::atomic::Ordering::Relaxed))
                / 1000;
            stats.tracked += 1;
            stats.max_age_secs = stats.max_age_secs.max(age);
            if age >= stale_after_secs {
                stats.stale += 1;
            }
        }
        stats
    }

    /// Seconds since the last heartbeat, or `None` when never bumped
    /// (treated as alive — never release an agent on missing data).
    pub fn heartbeat_age_secs(&self, session_id: &str) -> Option<u64> {
        let seen = self
            .heartbeats
            .get(session_id)
            .map(|v| v.load(std::sync::atomic::Ordering::Relaxed))?;
        Some(heartbeat_now_ms().saturating_sub(seen) / 1000)
    }

    /// Test-only: backdate a session's heartbeat.
    #[cfg(test)]
    pub(crate) fn backdate_heartbeat_for_test(&self, session_id: &str, age_secs: u64) {
        let now = heartbeat_now_ms();
        let slot = self
            .heartbeats
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(AtomicU64::new(now)))
            .clone();
        slot.store(
            now.saturating_sub(age_secs.saturating_mul(1000)),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Test-only: drop a session's heartbeat entirely.
    #[cfg(test)]
    pub(crate) fn clear_heartbeat_for_test(&self, session_id: &str) {
        self.heartbeats.remove(session_id);
    }

    fn notify_waiters(&self) {
        self.change_notify.notify_waiters();
    }

    pub fn upsert(&self, entry: ActiveProxyCallEntry, handle: SipSessionHandle) {
        // Emit dialog_created only for genuinely new sessions — upsert may be
        // called again for an existing session (e.g. media updates).
        let direction = entry.direction.clone();
        let is_new = self.entries.insert(entry.session_id.clone(), entry).is_none();
        if is_new {
            crate::metrics::sip::dialog_created(&direction);
        }
        self.handles.insert(handle.session_id().to_string(), handle);
        self.notify_waiters();
    }

    pub fn register_dialog(&self, dialog_id: String, handle: SipSessionHandle) {
        let session_key = handle.session_id().to_string();
        self.handles_by_dialog.insert(dialog_id.clone(), handle);
        use dashmap::mapref::entry::Entry;
        match self.dialog_by_session.entry(session_key) {
            Entry::Occupied(mut e) => {
                e.get_mut().push(dialog_id);
            }
            Entry::Vacant(e) => {
                e.insert(vec![dialog_id]);
            }
        }
    }

    pub fn unregister_dialog(&self, dialog_id: &str) {
        let handle = self.handles_by_dialog.remove(dialog_id);
        if let Some((_, handle)) = handle {
            let should_remove = {
                let mut dialogs = self.dialog_by_session.get_mut(handle.session_id());
                match dialogs.as_mut() {
                    Some(d) => {
                        d.retain(|d| d != dialog_id);
                        d.is_empty()
                    }
                    None => false,
                }
            };
            if should_remove {
                self.dialog_by_session.remove(handle.session_id());
            }
        }
    }

    pub fn get_handle_by_dialog(&self, dialog_id: &str) -> Option<SipSessionHandle> {
        self.handles_by_dialog.get(dialog_id).map(|e| e.clone())
    }

    pub fn register_call_id(&self, call_id: String, handle: SipSessionHandle) {
        self.register_dialog(call_id, handle);
    }

    pub fn get_handle_by_call_id(&self, call_id: &str) -> Option<SipSessionHandle> {
        self.get_handle_by_dialog(call_id)
    }

    pub fn unregister_call_id(&self, call_id: &str) {
        self.unregister_dialog(call_id);
    }

    pub fn register_dialog_identity(
        &self,
        dialog_id: &rsipstack::dialog::DialogId,
        handle: SipSessionHandle,
    ) {
        self.register_call_id(dialog_id.call_id.clone(), handle);
    }

    pub fn unregister_dialog_identity(&self, dialog_id: &rsipstack::dialog::DialogId) {
        self.unregister_call_id(&dialog_id.call_id);
    }

    pub fn update<F>(&self, session_id: &str, updater: F)
    where
        F: FnOnce(&mut ActiveProxyCallEntry),
    {
        if let Some(mut entry) = self.entries.get_mut(session_id) {
            updater(&mut *entry);
        }
        self.notify_waiters();
    }

    pub fn remove(&self, session_id: &str) {
        if let Some((_, entry)) = self.entries.remove(session_id) {
            crate::metrics::sip::dialog_terminated(&entry.direction, "hangup");
        }
        self.handles.remove(session_id);
        self.context_meta.remove(session_id);
        self.heartbeats.remove(session_id);
        if let Some((_, dialogs)) = self.dialog_by_session.remove(session_id) {
            for dialog_id in dialogs {
                self.handles_by_dialog.remove(&dialog_id);
            }
        }
    }

    pub fn set_context_meta(&self, session_id: String, meta: ActiveCallContextMeta) {
        self.context_meta.insert(session_id, meta);
    }

    pub fn get_context_meta(&self, session_id: &str) -> Option<ActiveCallContextMeta> {
        self.context_meta.get(session_id).map(|e| e.clone())
    }

    pub fn count(&self) -> usize {
        self.entries.len()
    }

    pub fn list_recent(&self, limit: usize) -> Vec<ActiveProxyCallEntry> {
        let mut entries: Vec<_> = self.entries.iter().map(|e| e.clone()).collect();
        entries.sort_by_key(|b| std::cmp::Reverse(b.started_at));
        if entries.len() > limit {
            entries.truncate(limit);
        }
        entries
    }

    pub fn get(&self, session_id: &str) -> Option<ActiveProxyCallEntry> {
        self.entries.get(session_id).map(|e| e.clone())
    }

    pub fn get_handle(&self, session_id: &str) -> Option<SipSessionHandle> {
        self.handles.get(session_id).map(|e| e.clone())
    }

    /// Get all active session IDs
    pub fn session_ids(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.key().clone()).collect()
    }

    /// Alias for count() for SessionRegistry compatibility
    pub fn len(&self) -> usize {
        self.count()
    }

    pub async fn wait_for_status(
        &self,
        session_id: &str,
        target: ActiveProxyCallStatus,
        timeout: std::time::Duration,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(entry) = self.get(session_id) {
                if entry.status == target {
                    return true;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::select! {
                _ = self.change_notify.notified() => {}
                _ = tokio::time::sleep_until(deadline) => return false,
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Register a unified session handle
    /// This is used by SipSession to register itself
    pub fn register_handle(&self, session_id: String, handle: SipSessionHandle) {
        let entry = ActiveProxyCallEntry {
            session_id: session_id.clone(),
            caller: None,
            callee: None,
            direction: "inbound".to_string(),
            started_at: Utc::now(),
            answered_at: None,
            status: ActiveProxyCallStatus::Ringing,
        };
        self.upsert(entry, handle);
    }

    pub fn handles_by_dialog_count(&self) -> usize {
        self.handles_by_dialog.len()
    }

    pub fn dialog_by_session_count(&self) -> usize {
        self.dialog_by_session.len()
    }

    /// Cleanup stale entries that have been inactive for longer than max_age
    /// Returns the number of entries removed
    pub fn cleanup_stale(&self, max_age: std::time::Duration) -> usize {
        let cutoff = Utc::now()
            - chrono::Duration::from_std(max_age).unwrap_or_else(|_| chrono::Duration::hours(1));

        let stale: Vec<(String, String)> = self
            .entries
            .iter()
            .filter(|entry| {
                let last_activity = entry.answered_at.unwrap_or(entry.started_at);
                last_activity < cutoff
            })
            .map(|entry| (entry.key().clone(), entry.direction.clone()))
            .collect();

        let count = stale.len();
        for (id, direction) in stale {
            self.entries.remove(&id);
            self.handles.remove(&id);
            self.context_meta.remove(&id);
            crate::metrics::sip::dialog_terminated(&direction, "stale_cleanup");
            if let Some((_, dialogs)) = self.dialog_by_session.remove(&id) {
                for dialog_id in dialogs {
                    self.handles_by_dialog.remove(&dialog_id);
                }
            }
        }

        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::proxy_call::sip_session::SipSession;

    fn make_handle(session_id: &str) -> SipSessionHandle {
        use crate::call::runtime::SessionId;

        let id = SessionId::from(session_id);
        let (handle, _cmd_rx) = SipSession::with_handle(id);
        handle
    }

    fn make_entry(session_id: &str) -> ActiveProxyCallEntry {
        ActiveProxyCallEntry {
            session_id: session_id.to_string(),
            caller: None,
            callee: None,
            direction: "outbound".to_string(),
            started_at: chrono::Utc::now(),
            answered_at: None,
            status: ActiveProxyCallStatus::Ringing,
        }
    }

    /// Heartbeat lifecycle: touch refresh, age, backdate/clear, remove cleanup.
    #[test]
    fn test_heartbeat_lifecycle() {
        let registry = ActiveProxyCallRegistry::new();
        let session = "session-heartbeat";

        // No heartbeat until the loop bumps it — unknown must read as None.
        assert!(registry.heartbeat_age_secs(session).is_none());

        registry.touch_heartbeat(session);
        assert!(registry.heartbeat_age_secs(session).is_some());
        assert!(registry.heartbeat_age_secs(session).unwrap() < 60);

        // Stale tier: backdated beyond the watchdog threshold reads as old.
        registry.backdate_heartbeat_for_test(session, 600);
        assert!(registry.heartbeat_age_secs(session).unwrap() >= 600);

        // Unknown tier: clearing makes the age None again (never judged dead).
        registry.clear_heartbeat_for_test(session);
        assert!(registry.heartbeat_age_secs(session).is_none());

        // remove() must not leak the heartbeat slot.
        let (handle, _cmd_rx) = {
            let id = crate::call::runtime::SessionId::from(session);
            SipSession::with_handle(id)
        };
        registry.upsert(
            ActiveProxyCallEntry {
                session_id: session.to_string(),
                caller: None,
                callee: None,
                direction: "inbound".to_string(),
                started_at: chrono::Utc::now(),
                answered_at: None,
                status: ActiveProxyCallStatus::Ringing,
            },
            handle,
        );
        registry.touch_heartbeat(session);
        assert!(registry.heartbeat_age_secs(session).is_some());
        registry.remove(session);
        assert!(registry.heartbeat_age_secs(session).is_none());
    }

    /// heartbeat_stats aggregates for the metrics sampler: tracked/max-age
    /// reflect the population, stale counts only ages beyond the threshold.
    #[test]
    fn test_heartbeat_stats_aggregates() {
        let registry = ActiveProxyCallRegistry::new();
        registry.touch_heartbeat("fresh-a");
        registry.touch_heartbeat("fresh-b");
        registry.backdate_heartbeat_for_test("stale-c", 300);

        let stats = registry.heartbeat_stats(120);
        assert_eq!(stats.tracked, 3);
        assert!(stats.max_age_secs >= 300);
        assert_eq!(stats.stale, 1);
    }

    /// Before fix: dialog_by_session stored only the LAST dialog, so remove() only
    /// cleaned the last entry — all previous handles_by_dialog entries leaked.
    /// After fix: all dialog ids are tracked and fully cleaned on remove().
    #[test]
    fn test_remove_cleans_all_dialog_handles() {
        let registry = ActiveProxyCallRegistry::new();
        let session = "session-1";
        let handle = make_handle(session);
        let entry = make_entry(session);

        // Simulate the sequence that happens during a real call:
        // 1. register_active_call  → registers server dialog
        registry.upsert(entry, handle.clone());
        registry.register_dialog("server-dialog".to_string(), handle.clone());
        assert_eq!(registry.handles_by_dialog_count(), 1);

        // 2. add_callee_dialog (trunk 1 attempt)
        registry.register_dialog("callee-dialog-1".to_string(), handle.clone());
        assert_eq!(registry.handles_by_dialog_count(), 2);

        // 3. add_callee_dialog (failover trunk 2)
        registry.register_dialog("callee-dialog-2".to_string(), handle.clone());
        assert_eq!(registry.handles_by_dialog_count(), 3);

        // All 3 dialogs should be tracked under this session
        assert_eq!(
            registry
                .dialog_by_session
                .get(session)
                .map(|e| e.len())
                .unwrap_or(0),
            3
        );

        // 4. Session ends → remove() must clean ALL three handles_by_dialog entries
        registry.remove(session);

        assert_eq!(registry.count(), 0, "entry should be gone");
        assert_eq!(
            registry.handles_by_dialog_count(),
            0,
            "all dialog handles must be cleaned up (was leaking before fix)"
        );
        assert_eq!(
            registry.dialog_by_session_count(),
            0,
            "dialog_by_session must be empty"
        );
    }

    /// Single-trunk call: server dialog + callee dialog → both must be cleaned.
    #[test]
    fn test_single_trunk_call_no_leak() {
        let registry = ActiveProxyCallRegistry::new();
        let session = "session-single";
        let handle = make_handle(session);

        registry.upsert(make_entry(session), handle.clone());
        registry.register_dialog("server-dlg".to_string(), handle.clone());
        registry.register_dialog("callee-dlg".to_string(), handle.clone());

        assert_eq!(registry.handles_by_dialog_count(), 2);

        registry.remove(session);

        assert_eq!(registry.handles_by_dialog_count(), 0);
        assert_eq!(registry.dialog_by_session_count(), 0);
    }

    /// unregister_dialog removes one dialog entry without touching others.
    #[test]
    fn test_unregister_dialog_partial() {
        let registry = ActiveProxyCallRegistry::new();
        let session = "session-partial";
        let handle = make_handle(session);

        registry.upsert(make_entry(session), handle.clone());
        registry.register_dialog("dlg-a".to_string(), handle.clone());
        registry.register_dialog("dlg-b".to_string(), handle.clone());

        // Unregister one
        registry.unregister_dialog("dlg-a");
        assert_eq!(registry.handles_by_dialog_count(), 1, "dlg-b should remain");

        // session still has 1 dialog tracked
        assert_eq!(
            registry
                .dialog_by_session
                .get(session)
                .map(|e| e.len())
                .unwrap_or(0),
            1
        );

        // Unregister second
        registry.unregister_dialog("dlg-b");
        assert_eq!(registry.handles_by_dialog_count(), 0);
        // session should be removed from dialog_by_session when empty
        assert_eq!(registry.dialog_by_session_count(), 0);
    }

    /// Multiple concurrent sessions should not interfere with each other.
    #[test]
    fn test_multiple_sessions_independent() {
        let registry = ActiveProxyCallRegistry::new();

        let h1 = make_handle("s1");
        let h2 = make_handle("s2");

        registry.upsert(make_entry("s1"), h1.clone());
        registry.upsert(make_entry("s2"), h2.clone());
        registry.register_dialog("s1-server".to_string(), h1.clone());
        registry.register_dialog("s1-callee".to_string(), h1.clone());
        registry.register_dialog("s2-server".to_string(), h2.clone());
        registry.register_dialog("s2-callee".to_string(), h2.clone());

        assert_eq!(registry.handles_by_dialog_count(), 4);

        // Remove session 1 — session 2 must be intact
        registry.remove("s1");
        assert_eq!(registry.count(), 1, "s2 still active");
        assert_eq!(
            registry.handles_by_dialog_count(),
            2,
            "only s2 dialogs remain"
        );

        registry.remove("s2");
        assert_eq!(registry.count(), 0);
        assert_eq!(registry.handles_by_dialog_count(), 0);
    }

    #[test]
    fn dialog_ownership_uses_bare_call_id() {
        let registry = ActiveProxyCallRegistry::new();
        let first = make_handle("first-session");
        let second = make_handle("second-session");
        let mut dialog = rsipstack::dialog::DialogId {
            call_id: "first-call".into(), local_tag: "local-tag".into(), remote_tag: String::new(),
        };
        registry.register_call_id(dialog.call_id.clone(), first.clone());
        assert_eq!(registry.get_handle_by_dialog(&dialog.call_id).unwrap().session_id(), "first-session");
        dialog.remote_tag = "remote-tag".into();
        registry.register_dialog_identity(&dialog, first.clone());
        dialog.local_tag = "another-local-tag".into();
        registry.register_dialog_identity(&dialog, first);
        assert_eq!(registry.handles_by_dialog_count(), 1, "tags must not create additional registry keys");
        assert_eq!(registry.get_handle_by_call_id(&dialog.call_id).unwrap().session_id(), "first-session");
        assert!(registry.get_handle_by_dialog(&dialog.to_string()).is_none());
        assert!(registry.get_handle_by_dialog(&format!("{}-{}", dialog.call_id, dialog.local_tag)).is_none());

        let other = rsipstack::dialog::DialogId { call_id: "second-call".into(), ..dialog.clone() };
        registry.register_dialog_identity(&other, second);
        registry.unregister_dialog_identity(&dialog);
        assert!(registry.get_handle_by_dialog(&dialog.call_id).is_none());
        assert!(registry.get_handle_by_call_id(&dialog.call_id).is_none());
        assert_eq!(registry.get_handle_by_dialog(&other.call_id).unwrap().session_id(), "second-session");
        registry.remove("second-session");
        registry.remove("first-session");
        assert_eq!(registry.handles_by_dialog_count(), 0);
        assert_eq!(registry.dialog_by_session_count(), 0);
    }

    #[test]
    fn test_context_meta_set_get_remove() {
        let registry = ActiveProxyCallRegistry::new();
        let session = "session-ctx";
        registry.upsert(make_entry(session), make_handle(session));
        registry.set_context_meta(
            session.to_string(),
            ActiveCallContextMeta {
                queue_id: Some("support".into()),
                queue_name: Some("Support".into()),
                skill_group_id: Some("support".into()),
                ..Default::default()
            },
        );
        let meta = registry.get_context_meta(session).expect("meta");
        assert_eq!(meta.queue_id.as_deref(), Some("support"));
        assert_eq!(meta.queue_name.as_deref(), Some("Support"));
        registry.remove(session);
        assert!(registry.get_context_meta(session).is_none());
    }
}
