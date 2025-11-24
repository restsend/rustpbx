use crate::call::DialDirection;
use crate::proxy::proxy_call::ProxyCallHandle;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ActiveProxyCallStatus {
    Ringing,
    Talking,
}

impl ActiveProxyCallStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Ringing => "Ringing",
            Self::Talking => "Talking",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ActiveProxyCallEntry {
    pub session_id: String,
    pub caller: Option<String>,
    pub callee: Option<String>,
    pub direction: String,
    pub started_at: DateTime<Utc>,
    pub answered_at: Option<DateTime<Utc>>,
    pub status: ActiveProxyCallStatus,
}

impl ActiveProxyCallEntry {
    pub fn status_label(&self) -> &'static str {
        self.status.label()
    }
}

#[derive(Default)]
struct RegistryState {
    entries: HashMap<String, ActiveProxyCallEntry>,
    handles: HashMap<String, ProxyCallHandle>,
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

    pub fn upsert(&self, entry: ActiveProxyCallEntry) {
        let mut guard = self.inner.lock().unwrap();
        guard.entries.insert(entry.session_id.clone(), entry);
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

    pub fn register_handle(&self, handle: ProxyCallHandle) {
        self.inner
            .lock()
            .unwrap()
            .handles
            .insert(handle.session_id().to_string(), handle);
    }

    pub fn get_handle(&self, session_id: &str) -> Option<ProxyCallHandle> {
        self.inner.lock().unwrap().handles.get(session_id).cloned()
    }
}

pub fn normalize_direction(direction: &DialDirection) -> String {
    match direction {
        DialDirection::Inbound => "Inbound".to_string(),
        DialDirection::Outbound => "Outbound".to_string(),
        DialDirection::Internal => "Internal".to_string(),
    }
}
