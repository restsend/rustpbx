use crate::call::DialDirection;
use crate::proxy::proxy_call::CallSessionHandle;
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
    pub tenant_id: Option<i64>,
}

#[derive(Default)]
struct RegistryState {
    entries: HashMap<String, ActiveProxyCallEntry>,
    handles: HashMap<String, CallSessionHandle>,
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

    pub fn get_handle(&self, session_id: &str) -> Option<CallSessionHandle> {
        self.inner.lock().unwrap().handles.get(session_id).cloned()
    }

    pub fn count_by_tenant(&self, tenant_id: i64) -> usize {
        self.inner
            .lock()
            .unwrap()
            .entries
            .values()
            .filter(|e| e.tenant_id == Some(tenant_id))
            .count()
    }
}

pub fn normalize_direction(direction: &DialDirection) -> String {
    match direction {
        DialDirection::Inbound => "Inbound".to_string(),
        DialDirection::Outbound => "Outbound".to_string(),
        DialDirection::Internal => "Internal".to_string(),
    }
}
