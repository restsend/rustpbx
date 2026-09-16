use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::rwi::TriggerInfo;

use super::provider::SessionEndTag;

/// Derive `duration_ms` from RFC3339 stamps: `end - start`, in whole
/// milliseconds.
///
/// Server-side contract for `ivr_step_trace`: every event that carries
/// `step_start_time` + `step_end_time` must also carry
/// `duration_ms == end - start`, so consumers can use the field directly
/// instead of re-deriving it. Returns `0` when `start` is `None`, either
/// stamp fails to parse, or the difference is negative (clock skew) —
/// mirroring the "unknown / instant" semantics of the previous hardcoded
/// zeros.
pub fn duration_ms_between(start: Option<&str>, end: &str) -> u64 {
    let Some(start) = start else {
        return 0;
    };
    let (Ok(s), Ok(e)) = (
        chrono::DateTime::parse_from_rfc3339(start),
        chrono::DateTime::parse_from_rfc3339(end),
    ) else {
        return 0;
    };
    (e - s).num_milliseconds().max(0) as u64
}

/// A single step trace entry for IVR step mode execution.
#[derive(Debug, Clone, Serialize)]
pub struct IvrTraceEntry {
    pub session_id: String,
    pub caller: String,
    pub callee: String,
    pub trigger: TriggerInfo,
    pub provider_url: Option<String>,
    pub action_type: String,
    pub action_json: Option<String>,
    pub duration_ms: u64,
    pub error: Option<String>,
    pub step_id: Option<String>,
    pub step_name: Option<String>,
    pub step_start_time: Option<String>,
    pub step_end_time: Option<String>,
    pub extra: Option<serde_json::Value>,
    /// Filled only on the session-end trace entry (the final row that records
    /// how the whole IVR session ended). `None` for ordinary step entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<SessionEndTag>,
    /// Companion detail for [`end_reason`](Self::end_reason) (e.g. transfer target).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_detail: Option<String>,
}

/// A summary of a trace session.
#[derive(Debug, Clone, Serialize)]
pub struct IvrTraceSession {
    pub session_id: String,
    pub caller: String,
    pub callee: String,
    pub direction: String,
    pub ivr_name: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub total_steps: u32,
    pub status: String, // "active" | "completed" | "error"
}

/// In-memory trace collector for step-mode IVR debugging.
pub struct IvrTraceCollector {
    entries: RwLock<VecDeque<IvrTraceEntry>>,
    sessions: RwLock<VecDeque<IvrTraceSession>>,
    max_entries: usize,
    max_sessions: usize,
}

impl IvrTraceCollector {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            entries: RwLock::new(VecDeque::with_capacity(5000)),
            sessions: RwLock::new(VecDeque::with_capacity(500)),
            max_entries: 5000,
            max_sessions: 500,
        })
    }

    pub async fn record_entry(&self, entry: IvrTraceEntry) {
        let mut entries = self.entries.write().await;
        if entries.len() >= self.max_entries {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    pub async fn record_session(&self, session: IvrTraceSession) {
        let mut sessions = self.sessions.write().await;
        if sessions.len() >= self.max_sessions {
            sessions.pop_front();
        }
        sessions.push_back(session);
    }

    pub async fn update_session_end(
        &self,
        session_id: &str,
        ended_at: DateTime<Utc>,
        status: &str,
    ) {
        let mut sessions = self.sessions.write().await;
        if let Some(s) = sessions
            .iter_mut()
            .find(|s: &&mut IvrTraceSession| s.session_id == session_id)
        {
            s.ended_at = Some(ended_at);
            s.status = status.to_string();
        }
    }

    pub async fn query_by_session(&self, session_id: &str) -> Vec<IvrTraceEntry> {
        let entries = self.entries.read().await;
        entries
            .iter()
            .filter(|e| e.session_id == session_id)
            .cloned()
            .collect()
    }

    pub async fn sessions(&self) -> Vec<IvrTraceSession> {
        self.sessions.read().await.iter().rev().cloned().collect()
    }

    pub async fn clear_session(&self, session_id: &str) {
        let mut entries = self.entries.write().await;
        entries.retain(|e| e.session_id != session_id);
        let mut sessions = self.sessions.write().await;
        sessions.retain(|s| s.session_id != session_id);
    }

    pub async fn increment_steps(&self, session_id: &str) {
        if let Some(s) = self
            .sessions
            .write()
            .await
            .iter_mut()
            .find(|s| s.session_id == session_id)
        {
            s.total_steps += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn test_duration_ms_between() {
        let start = "2026-09-14T04:13:30.100Z";
        let end = "2026-09-14T04:13:30.750Z";
        assert_eq!(duration_ms_between(Some(start), end), 650);

        // Mixed offsets parse to the same instants.
        assert_eq!(
            duration_ms_between(Some("2026-09-14T12:13:30.100+08:00"), end),
            650
        );

        // Missing / unparsable start → unknown, 0.
        assert_eq!(duration_ms_between(None, end), 0);
        assert_eq!(duration_ms_between(Some("not-a-timestamp"), end), 0);
        assert_eq!(duration_ms_between(Some(start), "also-bad"), 0);

        // Negative difference (clock skew) clamps to 0.
        assert_eq!(duration_ms_between(Some(end), start), 0);

        // Sub-millisecond windows truncate to 0 (instant steps).
        assert_eq!(
            duration_ms_between(
                Some("2026-09-14T04:13:30.766221Z"),
                "2026-09-14T04:13:30.766382Z"
            ),
            0
        );
    }

    fn mk_entry(session_id: &str, step: u32) -> IvrTraceEntry {
        IvrTraceEntry {
            session_id: session_id.to_string(),
            caller: "1001".to_string(),
            callee: "2000".to_string(),
            trigger: TriggerInfo::new("test"),
            provider_url: None,
            action_type: "Transfer".to_string(),
            action_json: None,
            duration_ms: 0,
            error: None,
            step_id: Some(step.to_string()),
            step_name: None,
            step_start_time: None,
            step_end_time: None,
            extra: None,
            end_reason: None,
            end_detail: None,
        }
    }

    fn mk_session(session_id: &str) -> IvrTraceSession {
        IvrTraceSession {
            session_id: session_id.to_string(),
            caller: "1001".to_string(),
            callee: "2000".to_string(),
            direction: "inbound".to_string(),
            ivr_name: Some("test_ivr".to_string()),
            started_at: Utc::now(),
            ended_at: None,
            total_steps: 0,
            status: "active".to_string(),
        }
    }

    #[tokio::test]
    async fn test_record_and_query_entry() {
        let collector = IvrTraceCollector::new();
        collector.record_entry(mk_entry("call_001", 0)).await;
        collector.record_entry(mk_entry("call_001", 1)).await;
        collector.record_entry(mk_entry("call_002", 0)).await;

        let entries_001 = collector.query_by_session("call_001").await;
        assert_eq!(entries_001.len(), 2);
        assert_eq!(entries_001[0].step_id.as_deref(), Some("0"));
        assert_eq!(entries_001[1].step_id.as_deref(), Some("1"));

        let entries_002 = collector.query_by_session("call_002").await;
        assert_eq!(entries_002.len(), 1);
    }

    #[tokio::test]
    async fn test_record_and_query_session() {
        let collector = IvrTraceCollector::new();
        collector.record_session(mk_session("call_001")).await;
        collector.record_session(mk_session("call_002")).await;

        let sessions = collector.sessions().await;
        assert_eq!(sessions.len(), 2);
        // sessions() returns in reverse order
        assert_eq!(sessions[0].session_id, "call_002");
    }

    #[tokio::test]
    async fn test_update_session_end() {
        let collector = IvrTraceCollector::new();
        collector.record_session(mk_session("call_001")).await;

        let now = Utc::now();
        collector
            .update_session_end("call_001", now, "completed")
            .await;

        let sessions = collector.sessions().await;
        let s = sessions
            .iter()
            .find(|s| s.session_id == "call_001")
            .unwrap();
        assert!(s.ended_at.is_some());
        assert_eq!(s.status, "completed");
    }

    #[tokio::test]
    async fn test_clear_session() {
        let collector = IvrTraceCollector::new();
        collector.record_entry(mk_entry("call_001", 0)).await;
        collector.record_entry(mk_entry("call_001", 1)).await;
        collector.record_session(mk_session("call_001")).await;

        collector.clear_session("call_001").await;
        assert!(collector.query_by_session("call_001").await.is_empty());
        assert!(
            collector
                .sessions()
                .await
                .iter()
                .find(|s| s.session_id == "call_001")
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_max_entries() {
        let collector = Arc::new(IvrTraceCollector {
            entries: RwLock::new(VecDeque::with_capacity(3)),
            sessions: RwLock::new(VecDeque::with_capacity(10)),
            max_entries: 3,
            max_sessions: 10,
        });
        for i in 0..5 {
            collector.record_entry(mk_entry("call_001", i)).await;
        }
        let entries = collector.query_by_session("call_001").await;
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].step_id.as_deref(), Some("2"));
        assert_eq!(entries[2].step_id.as_deref(), Some("4"));
    }

    #[tokio::test]
    async fn test_increment_steps() {
        let collector = IvrTraceCollector::new();
        collector.record_session(mk_session("call_001")).await;
        assert_eq!(
            collector
                .sessions()
                .await
                .iter()
                .find(|s| s.session_id == "call_001")
                .unwrap()
                .total_steps,
            0
        );
        collector.increment_steps("call_001").await;
        assert_eq!(
            collector
                .sessions()
                .await
                .iter()
                .find(|s| s.session_id == "call_001")
                .unwrap()
                .total_steps,
            1
        );
        collector.increment_steps("call_001").await;
        assert_eq!(
            collector
                .sessions()
                .await
                .iter()
                .find(|s| s.session_id == "call_001")
                .unwrap()
                .total_steps,
            2
        );
    }
}
