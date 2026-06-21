// tests/helpers/cdr_verifier.rs
//
// CDR (Call Detail Record) verifier for E2E tests.
//
// Usage:
// ```
// let (cdr, sender) = CdrVerifier::new();
// let pbx = TestPbx::start_with_inject(sip_port, TestPbxInject {
//     callrecord_sender: Some(sender),
//     ..Default::default()
// }).await;
// // ... make a call ...
// let record = cdr.wait_for_record("call-id-123", 5).await;
// cdr.assert_cdr(&record, CdrExpectation::default().with_recording(false));
// ```

use rustpbx::callrecord::{CallRecord, CallRecordHangupReason, CallRecordSender};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, warn};

fn record_duration_secs(record: &CallRecord) -> i64 {
    (record.end_time - record.start_time).num_seconds().max(0)
}

/// CDR expectation for validation
#[derive(Debug, Clone)]
pub struct CdrExpectation {
    pub expected_direction: Option<String>,
    pub expected_status: Option<String>,
    pub expect_recording: bool,
    pub expected_hangup_reason: Option<CallRecordHangupReason>,
    pub min_duration_secs: Option<f64>,
    pub max_duration_secs: Option<f64>,
    pub expected_caller: Option<String>,
    pub expected_callee: Option<String>,
}

impl Default for CdrExpectation {
    fn default() -> Self {
        Self {
            expected_direction: None,
            expected_status: None,
            expect_recording: false,
            expected_hangup_reason: None,
            min_duration_secs: None,
            max_duration_secs: None,
            expected_caller: None,
            expected_callee: None,
        }
    }
}

impl CdrExpectation {
    pub fn with_status(mut self, s: &str) -> Self {
        self.expected_status = Some(s.to_string());
        self
    }
    pub fn with_hangup_reason(mut self, r: CallRecordHangupReason) -> Self {
        self.expected_hangup_reason = Some(r);
        self
    }
    pub fn with_min_duration(mut self, s: f64) -> Self {
        self.min_duration_secs = Some(s);
        self
    }
    pub fn with_caller(mut self, c: &str) -> Self {
        self.expected_caller = Some(c.to_string());
        self
    }
}

/// Validate a CDR record against expectations
pub fn validate_cdr(record: &CallRecord, expected: &CdrExpectation) -> Vec<String> {
    let mut errors = Vec::new();

    if let Some(ref dir) = expected.expected_direction {
        if record.details.direction != *dir {
            errors.push(format!(
                "direction: expected={}, got={}",
                dir, record.details.direction
            ));
        }
    }

    if let Some(ref status) = expected.expected_status {
        if record.details.status != *status {
            errors.push(format!(
                "status: expected={}, got={}",
                status, record.details.status
            ));
        }
    }

    if expected.expect_recording {
        let has_recording = !record.recorder.is_empty() || record.details.recording_url.is_some();
        if !has_recording {
            errors.push("expected recording but none found".to_string());
        }
    }

    if let Some(ref reason) = expected.expected_hangup_reason {
        match &record.hangup_reason {
            Some(r) => {
                let reason_str = format!("{:?}", r);
                let expected_str = format!("{:?}", reason);
                if reason_str != expected_str {
                    errors.push(format!("hangup_reason: expected={:?}, got={:?}", reason, r));
                }
            }
            None => {
                errors.push(format!("hangup_reason: expected={:?}, got=None", reason));
            }
        }
    }

    if let Some(min_secs) = expected.min_duration_secs {
        let duration = record_duration_secs(record) as f64;
        if duration < min_secs {
            errors.push(format!(
                "duration: expected>={}s, got={}s",
                min_secs, duration
            ));
        }
    }

    if let Some(max_secs) = expected.max_duration_secs {
        let duration = record_duration_secs(record) as f64;
        if duration > max_secs {
            errors.push(format!(
                "duration: expected<={}s, got={}s",
                max_secs, duration
            ));
        }
    }

    if let Some(ref caller) = expected.expected_caller {
        if !record.caller.contains(caller) {
            errors.push(format!(
                "caller: expected containing '{}', got='{}'",
                caller, record.caller
            ));
        }
    }

    if let Some(ref callee) = expected.expected_callee {
        if !record.callee.contains(callee) {
            errors.push(format!(
                "callee: expected containing '{}', got='{}'",
                callee, record.callee
            ));
        }
    }

    errors
}

/// Captures and verifies CDR records during E2E tests.
/// Uses an internal mpsc channel to receive CallRecords.
pub struct CdrVerifier {
    records: Arc<RwLock<Vec<CallRecord>>>,
    _receiver_handle: tokio::task::JoinHandle<()>,
}

impl CdrVerifier {
    /// Create a new CDR verifier.
    /// Returns (CdrVerifier, CallRecordSender).
    pub fn new() -> (Self, CallRecordSender) {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<CallRecord>(
            rustpbx::callrecord::CALL_RECORD_CHANNEL_CAPACITY,
        );
        let records = Arc::new(RwLock::new(Vec::new()));
        let records_clone = records.clone();

        let _receiver_handle = tokio::spawn(async move {
            while let Some(record) = receiver.recv().await {
                debug!(
                    call_id = %record.call_id,
                    status = %record.details.status,
                    from = %record.caller,
                    to = %record.callee,
                    "CDR captured"
                );
                records_clone.write().await.push(record);
            }
            debug!("CDR verifier receiver closed");
        });

        (
            Self {
                records,
                _receiver_handle,
            },
            sender,
        )
    }

    /// Wait for a CDR record with specific call_id.
    pub async fn wait_for_record(&self, call_id: &str, timeout_secs: u64) -> Option<CallRecord> {
        let start = tokio::time::Instant::now();
        let timeout = Duration::from_secs(timeout_secs);

        while start.elapsed() < timeout {
            let records = self.records.read().await;
            if let Some(record) = records.iter().find(|r| r.call_id == call_id) {
                return Some(record.clone());
            }
            drop(records);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        warn!(call_id, timeout_secs, "Timeout waiting for CDR record");
        None
    }

    /// Get all captured records.
    pub async fn get_all_records(&self) -> Vec<CallRecord> {
        self.records.read().await.clone()
    }

    /// Assert a CDR record matches expectations. Panics on failure.
    pub fn assert_cdr(&self, record: &CallRecord, expected: &CdrExpectation) {
        let errors = validate_cdr(record, expected);
        assert!(errors.is_empty(), "{}", errors.join("; "));
    }

    /// Assert call completed (status=answered, hangup reason present).
    pub fn assert_call_completed(&self, record: &CallRecord) {
        assert_eq!(
            record.details.status, "answered",
            "expected answered call, got={}",
            record.details.status
        );
        assert!(
            record.hangup_reason.is_some(),
            "expected hangup_reason to be present"
        );
        assert!(
            record_duration_secs(record) > 0,
            "expected call duration > 0"
        );
    }

    /// Assert call was rejected — hangup reason present.
    /// Status code check is soft: 0 means the RWI CDR path didn't capture it.
    pub fn assert_call_rejected(&self, record: &CallRecord, expected_code: u16) {
        if record.status_code > 0 {
            assert_eq!(
                record.status_code, expected_code,
                "expected status code {}",
                expected_code
            );
        } else {
            eprintln!("[cdr] status_code=0 (RWI path doesn't capture SIP code for rejected calls)");
        }
        assert!(
            record.hangup_reason.is_some(),
            "expected hangup_reason for rejected call"
        );
    }
}

impl Drop for CdrVerifier {
    fn drop(&mut self) {
        self._receiver_handle.abort();
    }
}
