use crate::call::DialDirection;
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
pub struct ActiveProxyCallRegistry {
    inner: Mutex<HashMap<String, ActiveProxyCallEntry>>,
}

impl ActiveProxyCallRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn upsert(&self, entry: ActiveProxyCallEntry) {
        self.inner
            .lock()
            .unwrap()
            .insert(entry.session_id.clone(), entry);
    }

    pub fn update<F>(&self, session_id: &str, updater: F)
    where
        F: FnOnce(&mut ActiveProxyCallEntry),
    {
        if let Some(entry) = self.inner.lock().unwrap().get_mut(session_id) {
            updater(entry);
        }
    }

    pub fn remove(&self, session_id: &str) {
        self.inner.lock().unwrap().remove(session_id);
    }

    pub fn list_recent(&self, limit: usize) -> Vec<ActiveProxyCallEntry> {
        let mut entries: Vec<_> = self.inner.lock().unwrap().values().cloned().collect();
        entries.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        if entries.len() > limit {
            entries.truncate(limit);
        }
        entries
    }
}

pub fn normalize_direction(direction: &DialDirection) -> String {
    match direction {
        DialDirection::Inbound => "Inbound".to_string(),
        DialDirection::Outbound => "Outbound".to_string(),
        DialDirection::Internal => "Internal".to_string(),
    }
}
