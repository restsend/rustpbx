//! CDR (Call Detail Record) capture utilities for E2E testing

use crate::callrecord::CallRecordSender;
use crate::callrecord::{CallRecord, CallRecordHangupReason};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, warn};

/// Captures CDR records during testing
pub struct CdrCapture {
    records: Arc<RwLock<Vec<CallRecord>>>,
    sender: CallRecordSender,
    _receiver_handle: tokio::task::JoinHandle<()>,
}

impl CdrCapture {
    /// Create a new CDR capture with a channel
    pub fn new() -> (Self, CallRecordSender) {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<CallRecord>();
        let records = Arc::new(RwLock::new(Vec::new()));
        let records_clone = records.clone();

        let _receiver_handle = tokio::spawn(async move {
            while let Some(record) = receiver.recv().await {
                debug!(
                    call_id = %record.call_id,
                    status = %record.details.status,
                    "CDR captured"
                );
                records_clone.write().await.push(record);
            }
            debug!("CDR capture receiver closed");
        });

        (
            Self {
                records,
                sender: sender.clone(),
                _receiver_handle,
            },
            sender,
        )
    }

    /// Wait for a CDR record with specific call_id
    pub async fn wait_for_record(&self, call_id: &str, timeout: Duration) -> Option<CallRecord> {
        let start = tokio::time::Instant::now();

        while start.elapsed() < timeout {
            let records = self.records.read().await;
            if let Some(record) = records.iter().find(|r| r.call_id == call_id) {
                return Some(record.clone());
            }
            drop(records);

            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        warn!(call_id, "Timeout waiting for CDR record");
        None
    }

    /// Get all captured records
    pub async fn get_all_records(&self) -> Vec<CallRecord> {
        self.records.read().await.clone()
    }

    /// Find record by call_id
    pub async fn find_by_call_id(&self, call_id: &str) -> Option<CallRecord> {
        self.records
            .read()
            .await
            .iter()
            .find(|r| r.call_id == call_id)
            .cloned()
    }

    /// Clear all records
    pub async fn clear(&self) {
        self.records.write().await.clear();
    }

    /// Get the sender for use in server configuration
    pub fn sender(&self) -> CallRecordSender {
        self.sender.clone()
    }
}

impl Default for CdrCapture {
    fn default() -> Self {
        let (capture, sender) = Self::new();
        // Keep sender alive
        std::mem::forget(sender);
        capture
    }
}

/// CDR expectation for validation
#[derive(Debug, Clone)]
#[derive(Default)]
pub struct CdrExpectation {
    pub expected_direction: Option<String>,
    pub expected_status: Option<String>,
    pub expect_recording: Option<bool>,
    pub expected_hangup_reason: Option<CallRecordHangupReason>,
    pub min_duration_secs: Option<i64>,
    pub max_duration_secs: Option<i64>,
    pub expected_caller: Option<String>,
    pub expected_callee: Option<String>,
}


impl CdrExpectation {
    pub fn with_direction(mut self, direction: impl Into<String>) -> Self {
        self.expected_direction = Some(direction.into());
        self
    }

    pub fn with_status(mut self, status: impl Into<String>) -> Self {
        self.expected_status = Some(status.into());
        self
    }

    pub fn with_recording(mut self, has_recording: bool) -> Self {
        self.expect_recording = Some(has_recording);
        self
    }

    pub fn with_hangup_reason(mut self, reason: CallRecordHangupReason) -> Self {
        self.expected_hangup_reason = Some(reason);
        self
    }

    pub fn with_duration_range(mut self, min: i64, max: i64) -> Self {
        self.min_duration_secs = Some(min);
        self.max_duration_secs = Some(max);
        self
    }

    pub fn with_caller(mut self, caller: impl Into<String>) -> Self {
        self.expected_caller = Some(caller.into());
        self
    }

    pub fn with_callee(mut self, callee: impl Into<String>) -> Self {
        self.expected_callee = Some(callee.into());
        self
    }
}

/// CDR validation result
#[derive(Debug, Clone)]
pub struct CdrValidationResult {
    pub is_valid: bool,
    pub errors: Vec<String>,
}

impl CdrValidationResult {
    pub fn new() -> Self {
        Self {
            is_valid: true,
            errors: Vec::new(),
        }
    }

    pub fn add_error(&mut self, error: impl Into<String>) {
        self.errors.push(error.into());
        self.is_valid = false;
    }
}

impl Default for CdrValidationResult {
    fn default() -> Self {
        Self::new()
    }
}

/// Validate a CDR record against expectations
pub fn validate_cdr(record: &CallRecord, expected: &CdrExpectation) -> CdrValidationResult {
    let mut result = CdrValidationResult::new();

    // Validate direction
    if let Some(ref expected_dir) = expected.expected_direction
        && record.details.direction != *expected_dir {
            result.add_error(format!(
                "Direction mismatch: expected '{}', got '{}'",
                expected_dir, record.details.direction
            ));
        }

    // Validate status
    if let Some(ref expected_status) = expected.expected_status
        && record.details.status != *expected_status {
            result.add_error(format!(
                "Status mismatch: expected '{}', got '{}'",
                expected_status, record.details.status
            ));
        }

    // Validate recording
    if let Some(expect_recording) = expected.expect_recording {
        let has_recording = !record.recorder.is_empty() || record.details.recording_url.is_some();
        if has_recording != expect_recording {
            result.add_error(format!(
                "Recording mismatch: expected recording={}, got recording={}",
                expect_recording, has_recording
            ));
        }
    }

    // Validate hangup reason
    if let Some(ref expected_reason) = expected.expected_hangup_reason {
        match &record.hangup_reason {
            Some(actual) if actual == expected_reason => {}
            Some(actual) => {
                result.add_error(format!(
                    "Hangup reason mismatch: expected '{:?}', got '{:?}'",
                    expected_reason, actual
                ));
            }
            None => {
                result.add_error(format!(
                    "Hangup reason missing: expected '{:?}'",
                    expected_reason
                ));
            }
        }
    }

    // Validate duration
    let duration = (record.end_time - record.start_time).num_seconds();
    if let Some(min) = expected.min_duration_secs
        && duration < min {
            result.add_error(format!(
                "Duration too short: expected >= {}s, got {}s",
                min, duration
            ));
        }
    if let Some(max) = expected.max_duration_secs
        && duration > max {
            result.add_error(format!(
                "Duration too long: expected <= {}s, got {}s",
                max, duration
            ));
        }

    // Validate caller
    if let Some(ref expected_caller) = expected.expected_caller
        && !record.caller.contains(expected_caller) {
            result.add_error(format!(
                "Caller mismatch: expected containing '{}', got '{}'",
                expected_caller, record.caller
            ));
        }

    // Validate callee
    if let Some(ref expected_callee) = expected.expected_callee
        && !record.callee.contains(expected_callee) {
            result.add_error(format!(
                "Callee mismatch: expected containing '{}', got '{}'",
                expected_callee, record.callee
            ));
        }

    // Basic completeness checks
    if record.call_id.is_empty() {
        result.add_error("Call ID is empty");
    }
    if record.start_time.timestamp() == 0 {
        result.add_error("Start time is not set");
    }
    if record.end_time.timestamp() == 0 {
        result.add_error("End time is not set");
    }

    result
}

/// Assertion helpers for tests
pub mod assertions {
    use super::*;

    /// Assert that CDR record matches expectations
    pub fn assert_cdr_matches(record: &CallRecord, expected: &CdrExpectation) {
        let result = validate_cdr(record, expected);
        if !result.is_valid {
            panic!("CDR validation failed:\n{}", result.errors.join("\n"));
        }
    }

    /// Assert that call was completed normally
    pub fn assert_call_completed(record: &CallRecord) {
        let expected = CdrExpectation::default()
            .with_status("completed")
            .with_hangup_reason(CallRecordHangupReason::ByCaller);
        assert_cdr_matches(record, &expected);
    }

    /// Assert that call was rejected
    pub fn assert_call_rejected(record: &CallRecord, expected_status_code: u16) {
        assert_eq!(
            record.status_code, expected_status_code,
            "Expected status code {}, got {}",
            expected_status_code, record.status_code
        );

        match record.hangup_reason {
            Some(CallRecordHangupReason::Rejected) | Some(CallRecordHangupReason::Canceled) => {}
            ref other => {
                panic!("Expected rejected/canceled reason, got {:?}", other);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn create_test_record() -> CallRecord {
        CallRecord {
            call_id: "test-call-123".to_string(),
            start_time: Utc::now(),
            ring_time: Some(Utc::now()),
            answer_time: Some(Utc::now()),
            end_time: Utc::now() + chrono::Duration::seconds(10),
            caller: "sip:alice@example.com".to_string(),
            callee: "sip:bob@example.com".to_string(),
            status_code: 200,
            hangup_reason: Some(CallRecordHangupReason::ByCaller),
            hangup_messages: vec![],
            recorder: vec![],
            sip_leg_roles: Default::default(),
            leg_timeline: Default::default(),
            details: crate::callrecord::CallDetails {
                direction: "outbound".to_string(),
                status: "completed".to_string(),
                from_number: Some("alice".to_string()),
                to_number: Some("bob".to_string()),
                caller_name: None,
                agent_name: None,
                queue: None,
                department_id: None,
                extension_id: None,
                sip_trunk_id: None,
                route_id: None,
                sip_gateway: None,
                recording_url: None,
                recording_duration_secs: None,
                has_transcript: false,
                transcript_status: None,
                transcript_language: None,
                tags: None,
                rewrite: Default::default(),
                last_error: None,
                metadata: None,
            },
            extensions: http::Extensions::new(),
        }
    }

    #[tokio::test]
    async fn test_cdr_capture() {
        let (capture, sender) = CdrCapture::new();

        let mut record = create_test_record();
        record.call_id = "test-456".to_string();

        sender.send(record.clone()).unwrap();

        let found = capture
            .wait_for_record("test-456", Duration::from_secs(1))
            .await;
        assert!(found.is_some());
        assert_eq!(found.unwrap().call_id, "test-456");
    }

    #[test]
    fn test_validate_cdr_success() {
        let record = create_test_record();
        let expected = CdrExpectation::default()
            .with_direction("outbound")
            .with_status("completed")
            .with_hangup_reason(CallRecordHangupReason::ByCaller)
            .with_caller("alice")
            .with_callee("bob")
            .with_duration_range(5, 15);

        let result = validate_cdr(&record, &expected);
        assert!(result.is_valid, "Errors: {:?}", result.errors);
    }

    #[test]
    fn test_validate_cdr_failure() {
        let record = create_test_record();
        let expected = CdrExpectation::default()
            .with_direction("inbound") // Wrong direction
            .with_status("failed"); // Wrong status

        let result = validate_cdr(&record, &expected);
        assert!(!result.is_valid);
        assert_eq!(result.errors.len(), 2);
    }
}
