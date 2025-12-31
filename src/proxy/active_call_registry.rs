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
    dialog_by_session: HashMap<String, String>,
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
            .insert(handle.session_id().to_string(), dialog_id.clone());
        guard.handles_by_dialog.insert(dialog_id, handle);
    }

    pub fn unregister_dialog(&self, dialog_id: &str) {
        let mut guard = self.inner.lock().unwrap();
        if let Some(handle) = guard.handles_by_dialog.remove(dialog_id) {
            guard.dialog_by_session.remove(handle.session_id());
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
        if let Some(dialog_id) = guard.dialog_by_session.remove(session_id) {
            guard.handles_by_dialog.remove(&dialog_id);
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
}
