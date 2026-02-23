use crate::proxy::proxy_call::state::CallSessionHandle;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize)]
pub enum ActiveProxyCallStatus {
    Ringing,
    Talking,
}

impl ToString for ActiveProxyCallStatus {
    fn to_string(&self) -> String {
        match self {
            ActiveProxyCallStatus::Ringing => "ringing".to_string(),
            ActiveProxyCallStatus::Talking => "talking".to_string(),
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

#[derive(Default)]
struct RegistryState {
    entries: HashMap<String, ActiveProxyCallEntry>,
    handles: HashMap<String, CallSessionHandle>,
    handles_by_dialog: HashMap<String, CallSessionHandle>,
    // session_id -> all registered dialog_ids (multiple dialogs per session during failover)
    dialog_by_session: HashMap<String, Vec<String>>,
}

pub struct ActiveProxyCallRegistry {
    inner: Mutex<RegistryState>,
}

impl ActiveProxyCallRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(RegistryState::default()),
        }
    }

    pub fn upsert(&self, entry: ActiveProxyCallEntry, handle: CallSessionHandle) {
        let mut guard = self.inner.lock().unwrap();
        guard.entries.insert(entry.session_id.clone(), entry);
        guard
            .handles
            .insert(handle.session_id().to_string(), handle);
    }

    pub fn register_dialog(&self, dialog_id: String, handle: CallSessionHandle) {
        let mut guard = self.inner.lock().unwrap();
        guard
            .dialog_by_session
            .entry(handle.session_id().to_string())
            .or_default()
            .push(dialog_id.clone());
        guard.handles_by_dialog.insert(dialog_id, handle);
    }

    pub fn unregister_dialog(&self, dialog_id: &str) {
        let mut guard = self.inner.lock().unwrap();
        if let Some(handle) = guard.handles_by_dialog.remove(dialog_id) {
            if let Some(dialogs) = guard.dialog_by_session.get_mut(handle.session_id()) {
                dialogs.retain(|d| d != dialog_id);
                if dialogs.is_empty() {
                    guard.dialog_by_session.remove(handle.session_id());
                }
            }
        }
    }

    pub fn get_handle_by_dialog(&self, dialog_id: &str) -> Option<CallSessionHandle> {
        let guard = self.inner.lock().unwrap();
        guard.handles_by_dialog.get(dialog_id).cloned()
    }

    pub fn update<F>(&self, session_id: &str, updater: F)
    where
        F: FnOnce(&mut ActiveProxyCallEntry),
    {
        if let Some(entry) = self.inner.lock().unwrap().entries.get_mut(session_id) {
            updater(entry);
        }
    }

    pub fn remove(&self, session_id: &str) {
        let mut guard = self.inner.lock().unwrap();
        guard.entries.remove(session_id);
        guard.handles.remove(session_id);
        // Remove all dialog handles registered for this session
        if let Some(dialog_ids) = guard.dialog_by_session.remove(session_id) {
            for dialog_id in dialog_ids {
                guard.handles_by_dialog.remove(&dialog_id);
            }
        }
    }

    pub fn count(&self) -> usize {
        self.inner.lock().unwrap().entries.len()
    }

    pub fn list_recent(&self, limit: usize) -> Vec<ActiveProxyCallEntry> {
        let mut entries: Vec<_> = self
            .inner
            .lock()
            .unwrap()
            .entries
            .values()
            .cloned()
            .collect();
        entries.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        if entries.len() > limit {
            entries.truncate(limit);
        }
        entries
    }

    pub fn get(&self, session_id: &str) -> Option<ActiveProxyCallEntry> {
        self.inner.lock().unwrap().entries.get(session_id).cloned()
    }

    pub fn get_handle(&self, session_id: &str) -> Option<CallSessionHandle> {
        self.inner.lock().unwrap().handles.get(session_id).cloned()
    }

    #[cfg(test)]
    pub fn handles_by_dialog_count(&self) -> usize {
        self.inner.lock().unwrap().handles_by_dialog.len()
    }

    #[cfg(test)]
    pub fn dialog_by_session_count(&self) -> usize {
        self.inner.lock().unwrap().dialog_by_session.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::DialDirection;
    use crate::proxy::proxy_call::state::{CallSessionHandle, CallSessionShared};

    fn make_handle(session_id: &str) -> CallSessionHandle {
        let shared = CallSessionShared::new(
            session_id.to_string(),
            DialDirection::Outbound,
            Some("caller".to_string()),
            Some("callee".to_string()),
            None,
        );
        let (handle, _rx) = CallSessionHandle::with_shared(shared);
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
            registry.inner.lock().unwrap().dialog_by_session[session].len(),
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
            registry.inner.lock().unwrap().dialog_by_session[session].len(),
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
}
