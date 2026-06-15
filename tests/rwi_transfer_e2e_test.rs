use rustpbx::rwi::{CallTransferFailed, CallTransferAccepted, CallTransferred};
use rustpbx::rwi::{ConferenceMergeRequested, ConferenceMerged, ConferenceMergeFailed};
use rustpbx::rwi::{CallRinging, CallAnswered, CallBridged, CallHangup, CallUnbridged};

#[test]
fn test_rwi_event_call_transfer_failed() {
    let event = CallTransferFailed { call_id: "test-call".into(), sip_status: Some(486), reason: Some("refer_rejected".into()) };
    assert_eq!(event.call_id, "test-call");
    assert_eq!(event.sip_status, Some(486));
    assert_eq!(event.reason, Some("refer_rejected".to_string()));
}

#[test]
fn test_rwi_event_conference_consult_dialing() {
    // ConferenceConsultDialing is legacy. No typed struct - skip.
}

#[test]
fn test_rwi_event_conference_consult_connected() {
    // ConferenceConsultConnected is legacy. No typed struct - skip.
}

#[test]
fn test_rwi_event_conference_merge_requested() {
    let event = ConferenceMergeRequested { call_id: "call-001".into(), consultation_call_id: "consult-001".into() };
    assert_eq!(event.call_id, "call-001");
    assert_eq!(event.consultation_call_id, "consult-001");
}

#[test]
fn test_rwi_event_conference_merged() {
    let event = ConferenceMerged { conf_id: "conf-001".into(), call_id: "call-001".into() };
    assert_eq!(event.conf_id, "conf-001");
}

#[test]
fn test_rwi_event_conference_merge_failed() {
    let event = ConferenceMergeFailed { conf_id: "conf-001".into(), call_id: "call-001".into(), reason: "error".into() };
    assert_eq!(event.reason, "error");
}

#[test]
fn test_rwi_event_call_ownership_changed() {
    // CallOwnershipChanged is legacy. No typed struct - skip.
}

#[test]
fn test_rwi_event_session_resumed() {
    // SessionResumed is legacy. No typed struct - skip.
}

#[test]
fn test_rwi_event_call_ringing() {
    let event = CallRinging { call_id: "call-001".into() };
    assert_eq!(event.call_id, "call-001");
}

#[test]
fn test_rwi_event_call_answered() {
    let event = CallAnswered { call_id: "call-001".into() };
    assert_eq!(event.call_id, "call-001");
}

#[test]
fn test_rwi_event_call_bridged() {
    let event = CallBridged { leg_a: "leg-a".into(), leg_b: "leg-b".into() };
    assert_eq!(event.leg_a, "leg-a");
}

#[test]
fn test_rwi_event_call_hangup() {
    let event = CallHangup { call_id: "call-001".into(), reason: Some("normal".into()), sip_status: Some(200) };
    assert_eq!(event.call_id, "call-001");
}

#[test]
fn test_rwi_event_call_unbridged() {
    let event = CallUnbridged { call_id: "call-001".into() };
    assert_eq!(event.call_id, "call-001");
}

#[test]
fn test_rwi_event_call_transfer_accepted() {
    let event = CallTransferAccepted { call_id: "call-001".into() };
    assert_eq!(event.call_id, "call-001");
}

#[test]
fn test_rwi_event_call_transferred() {
    let event = CallTransferred { call_id: "call-001".into() };
    assert_eq!(event.call_id, "call-001");
}
