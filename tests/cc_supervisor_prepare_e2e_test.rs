// tests/cc_supervisor_prepare_e2e_test.rs
//
// End-to-end tests for CC supervisor prepare/redeem/escalate flow.
// Tests the full lifecycle of the SupervisorManager.

use rustpbx::addons::cc::supervisor::{MonitorType, SupervisorManager};

#[test]
fn test_prepare_redeem_lifecycle() {
    let mut mgr = SupervisorManager::new();

    let (uri, session_id) = mgr.prepare_monitor(
        "sup-1".to_string(),
        "call-abc".to_string(),
        "callee".to_string(),
        MonitorType::Listen,
    );

    assert!(uri.starts_with("monitor#"), "URI should start with monitor#");
    assert!(session_id.starts_with("mon-"), "Session ID should start with mon-");

    let token = uri.strip_prefix("monitor#").unwrap();
    assert!(uuid::Uuid::parse_str(token).is_ok(), "Token should be a valid UUID");

    let prepare = mgr.get_prepare(token).unwrap();
    assert_eq!(prepare.target_call_id, "call-abc");
    assert_eq!(prepare.supervisor_id, "sup-1");
    assert_eq!(prepare.monitor_type, MonitorType::Listen);
    assert!(!prepare.is_expired());

    let result = mgr.redeem_prepare(token, "sup-session-1").unwrap();
    assert_eq!(result, session_id);

    assert!(mgr.get_prepare(token).is_none());

    let session = mgr.get_session(&session_id).unwrap();
    assert_eq!(session.monitor_type, MonitorType::Listen);
    assert_eq!(session.supervisor_call_id, Some("sup-session-1".to_string()));

    mgr.escalate(&session_id, MonitorType::Whisper).unwrap();
    assert_eq!(mgr.get_session(&session_id).unwrap().monitor_type, MonitorType::Whisper);

    mgr.escalate(&session_id, MonitorType::Barge).unwrap();
    assert_eq!(mgr.get_session(&session_id).unwrap().monitor_type, MonitorType::Barge);

    let stopped = mgr.stop_monitor(&session_id, "manual_stop");
    assert!(stopped.is_some());
    assert_eq!(mgr.active_count(), 0);
    assert_eq!(mgr.pending_records().len(), 1);
}

#[test]
fn test_prepare_expires() {
    let mut mgr = SupervisorManager::new().with_prepare_ttl(0);

    let (uri, _) = mgr.prepare_monitor(
        "sup-1".to_string(),
        "call-1".to_string(),
        "callee".to_string(),
        MonitorType::Listen,
    );

    let token = uri.strip_prefix("monitor#").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));

    let result = mgr.redeem_prepare(token, "session-1");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("expired"));
}

#[test]
fn test_double_redeem_fails() {
    let mut mgr = SupervisorManager::new();

    let (uri, _) = mgr.prepare_monitor(
        "sup-1".to_string(),
        "call-1".to_string(),
        "callee".to_string(),
        MonitorType::Listen,
    );

    let token = uri.strip_prefix("monitor#").unwrap();

    mgr.redeem_prepare(token, "session-1").unwrap();

    let result = mgr.redeem_prepare(token, "session-2");
    assert!(result.is_err());
}

#[test]
fn test_escalate_wrong_direction() {
    let mut mgr = SupervisorManager::new();

    mgr.start_monitor(
        "mon-1".to_string(),
        "sup-1".to_string(),
        "call-1".to_string(),
        "callee".to_string(),
        MonitorType::Listen,
        None,
    )
    .unwrap();

    let result = mgr.escalate("mon-1", MonitorType::Listen);
    assert!(result.is_err());

    mgr.escalate("mon-1", MonitorType::Whisper).unwrap();
    mgr.escalate("mon-1", MonitorType::Barge).unwrap();

    let result = mgr.escalate("mon-1", MonitorType::Whisper);
    assert!(result.is_err());
}

#[test]
fn test_multiple_monitors_same_call() {
    let mut mgr = SupervisorManager::new();

    let (uri1, _sid1) = mgr.prepare_monitor(
        "sup-1".to_string(),
        "call-1".to_string(),
        "callee".to_string(),
        MonitorType::Listen,
    );
    let (uri2, _sid2) = mgr.prepare_monitor(
        "sup-2".to_string(),
        "call-1".to_string(),
        "callee".to_string(),
        MonitorType::Barge,
    );

    let token1 = uri1.strip_prefix("monitor#").unwrap();
    let token2 = uri2.strip_prefix("monitor#").unwrap();

    mgr.redeem_prepare(token1, "sup-session-1").unwrap();
    mgr.redeem_prepare(token2, "sup-session-2").unwrap();

    assert_eq!(mgr.active_count(), 2);

    let stopped = mgr.stop_by_target_call("call-1", "call_end");
    assert_eq!(stopped.len(), 2);
    assert_eq!(mgr.active_count(), 0);
}

#[test]
fn test_prepare_unique_tokens() {
    let mut mgr = SupervisorManager::new();

    let (uri1, _) = mgr.prepare_monitor(
        "sup-1".to_string(),
        "call-1".to_string(),
        "callee".to_string(),
        MonitorType::Listen,
    );
    let (uri2, _) = mgr.prepare_monitor(
        "sup-2".to_string(),
        "call-2".to_string(),
        "callee".to_string(),
        MonitorType::Whisper,
    );

    assert_ne!(uri1, uri2, "Each prepare should generate a unique token");
}
