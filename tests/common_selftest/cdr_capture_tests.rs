//! Self-tests for `tests/common/cdr_capture.rs`.
//! Extracted from the module's embedded `#[cfg(test)] mod tests` so the
//! harness self-tests run ONCE here instead of in every aggregator
//! binary (they used to re-execute ~11x per `cargo test` run).

use crate::common::cdr_capture::*;
use chrono::Utc;
use rustpbx::callrecord::{CallRecord, CallRecordHangupReason};
use std::time::Duration;

fn create_test_record() -> CallRecord {
    CallRecord {
        call_id: "test-call-123".to_string(),
        session_id: None,
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
        details: rustpbx::callrecord::CallDetails {
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
            outbound_sip_trunk_id: None,
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
            cdr_file_path: None,
        },
        extensions: http::Extensions::new(),
    }
}

#[tokio::test]
async fn test_cdr_capture() {
    let (capture, sender) = CdrCapture::new();

    let mut record = create_test_record();
    record.call_id = "test-456".to_string();

    sender.send(record.clone()).await.unwrap();

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
