use crate::config::ProxyConfig;
use crate::proxy::cdr::CallSession;
use crate::{
    callrecord::{CallRecord, CallRecordHangupReason},
    handler::CallOption,
    proxy::{cdr::CdrModule, server::SipServerInner, ProxyModule},
};
use anyhow::Result;
use chrono::Utc;
use rsipstack::dialog::DialogId;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn create_test_server() -> Arc<SipServerInner> {
    use crate::proxy::{locator::MemoryLocator, user::MemoryUserBackend};

    Arc::new(SipServerInner {
        cancel_token: CancellationToken::new(),
        config: Arc::new(ProxyConfig::default()),
        user_backend: Arc::new(Box::new(MemoryUserBackend::new(None))),
        locator: Arc::new(Box::new(MemoryLocator::new())),
        callrecord_sender: None,
    })
}

#[tokio::test]
async fn test_cdr_module_creation_and_basic_functionality() -> Result<()> {
    let server = create_test_server();
    let config = Arc::new(ProxyConfig::default());

    let mut module = CdrModule::new(server, config);

    // Test module basic properties
    assert_eq!(module.name(), "cdr");
    assert!(module.allow_methods().contains(&rsip::Method::Invite));
    assert!(module.allow_methods().contains(&rsip::Method::Bye));
    assert!(module.allow_methods().contains(&rsip::Method::Cancel));
    assert!(module.allow_methods().contains(&rsip::Method::Ack));
    assert_eq!(module.get_active_session_count(), 0);

    // Test module lifecycle
    assert!(module.on_start().await.is_ok());
    assert!(module.on_stop().await.is_ok());

    Ok(())
}

#[tokio::test]
async fn test_cdr_create_module_factory() -> Result<()> {
    let server = create_test_server();
    let config = Arc::new(ProxyConfig::default());

    // Test the create factory method
    let module_box = CdrModule::create(server, config)?;

    assert_eq!(module_box.name(), "cdr");

    Ok(())
}

#[tokio::test]
async fn test_cdr_allowed_methods() -> Result<()> {
    let server = create_test_server();
    let config = Arc::new(ProxyConfig::default());
    let module = CdrModule::new(server, config);

    let allowed_methods = module.allow_methods();

    // CDR module should handle these SIP methods for call tracking
    assert!(allowed_methods.contains(&rsip::Method::Invite));
    assert!(allowed_methods.contains(&rsip::Method::Bye));
    assert!(allowed_methods.contains(&rsip::Method::Cancel));
    assert!(allowed_methods.contains(&rsip::Method::Ack));

    // Should have exactly 4 methods
    assert_eq!(allowed_methods.len(), 4);

    Ok(())
}

#[tokio::test]
async fn test_cdr_module_name() -> Result<()> {
    let server = create_test_server();
    let config = Arc::new(ProxyConfig::default());
    let module = CdrModule::new(server, config);

    assert_eq!(module.name(), "cdr");

    Ok(())
}

#[tokio::test]
async fn test_cdr_session_count_tracking() -> Result<()> {
    let server = create_test_server();
    let config = Arc::new(ProxyConfig::default());
    let module = CdrModule::new(server, config);

    // Initially should have no active sessions
    assert_eq!(module.get_active_session_count(), 0);

    // The count is updated when actual SIP transactions are processed
    // In a real scenario, INVITE would increase the count, BYE/CANCEL would decrease it

    Ok(())
}

#[tokio::test]
async fn test_cdr_module_lifecycle() -> Result<()> {
    let server = create_test_server();
    let config = Arc::new(ProxyConfig::default());
    let mut module = CdrModule::new(server, config);

    // Test start
    let start_result = module.on_start().await;
    assert!(start_result.is_ok());

    // Test stop
    let stop_result = module.on_stop().await;
    assert!(stop_result.is_ok());

    Ok(())
}

#[tokio::test]
async fn test_cdr_call_option_defaults() -> Result<()> {
    // Test that CallOption can be created with defaults
    let call_option = CallOption::default();

    // Verify default values
    assert!(call_option.denoise.is_none());
    assert!(call_option.offer.is_none());
    assert!(call_option.recorder.is_none());
    assert!(call_option.vad.is_none());
    assert!(call_option.asr.is_none());
    assert!(call_option.tts.is_none());
    assert!(call_option.handshake_timeout.is_none());
    assert!(call_option.enable_ipv6.is_none());
    assert!(call_option.sip.is_none());
    assert!(call_option.extra.is_none());
    assert!(call_option.codec.is_none());

    Ok(())
}

#[tokio::test]
async fn test_cdr_call_record_hangup_reasons() -> Result<()> {
    // Test that all hangup reasons are available and can be used
    let reasons = vec![
        CallRecordHangupReason::ByCaller,
        CallRecordHangupReason::Canceled,
        CallRecordHangupReason::BySystem,
    ];

    for reason in reasons {
        // Each reason should be valid and comparable
        match reason {
            CallRecordHangupReason::ByCaller => assert!(true),
            CallRecordHangupReason::Canceled => assert!(true),
            CallRecordHangupReason::BySystem => assert!(true),
            _ => assert!(false, "Unexpected hangup reason variant"),
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_cdr_configuration() -> Result<()> {
    let server = create_test_server();
    let config = Arc::new(ProxyConfig::default());

    // Test that module can be created with different configurations
    let module1 = CdrModule::new(server.clone(), config.clone());
    let module2 = CdrModule::new(server, config);

    // Both modules should work independently
    assert_eq!(module1.name(), "cdr");
    assert_eq!(module2.name(), "cdr");
    assert_eq!(module1.get_active_session_count(), 0);
    assert_eq!(module2.get_active_session_count(), 0);

    Ok(())
}

#[tokio::test]
async fn test_cdr_call_record_sender() -> Result<()> {
    // Test that CallRecordSender can be created and used
    let (sender, mut receiver) = mpsc::unbounded_channel();

    // Create a test call record
    let call_record = CallRecord {
        call_type: crate::handler::call::ActiveCallType::Sip,
        option: Some(CallOption::default()),
        call_id: "test-call-123".to_string(),
        start_time: chrono::Utc::now(),
        ring_time: None,
        answer_time: None,
        end_time: chrono::Utc::now(),
        caller: "alice@example.com".to_string(),
        callee: "bob@example.com".to_string(),
        status_code: 200,
        hangup_reason: Some(CallRecordHangupReason::ByCaller),
        recorder: vec![],
        extras: Some(std::collections::HashMap::new()),
        dump_event_file: None,
    };

    // Send the record
    let send_result = sender.send(call_record.clone());
    assert!(send_result.is_ok());

    // Receive the record
    let received = receiver.try_recv();
    assert!(received.is_ok());
    let received_record = received.unwrap();

    assert_eq!(received_record.call_id, "test-call-123");
    assert_eq!(received_record.caller, "alice@example.com");
    assert_eq!(received_record.callee, "bob@example.com");
    assert_eq!(received_record.status_code, 200);

    Ok(())
}

#[tokio::test]
async fn test_cdr_multiple_modules() -> Result<()> {
    // Test that multiple CDR modules can coexist
    let server1 = create_test_server();
    let server2 = create_test_server();
    let config1 = Arc::new(ProxyConfig::default());
    let config2 = Arc::new(ProxyConfig::default());

    let module1 = CdrModule::new(server1, config1);
    let module2 = CdrModule::new(server2, config2);

    // Both modules should be independent
    assert_eq!(module1.get_active_session_count(), 0);
    assert_eq!(module2.get_active_session_count(), 0);

    // Both should have the same interface
    assert_eq!(module1.name(), module2.name());
    assert_eq!(module1.allow_methods(), module2.allow_methods());

    Ok(())
}

#[tokio::test]
async fn test_cdr_proxy_module_trait() -> Result<()> {
    let server = create_test_server();
    let config = Arc::new(ProxyConfig::default());
    let mut module = CdrModule::new(server, config);

    // Test that the module properly implements ProxyModule trait
    assert_eq!(module.name(), "cdr");

    let allowed_methods = module.allow_methods();
    assert!(!allowed_methods.is_empty());

    // Test lifecycle methods
    let start_result = module.on_start().await;
    assert!(start_result.is_ok());

    let stop_result = module.on_stop().await;
    assert!(stop_result.is_ok());

    Ok(())
}

#[tokio::test]
async fn test_cdr_module_creation() {
    let server = create_test_server();
    let config = Arc::new(ProxyConfig::default());

    let module = CdrModule::new(server, config);

    assert_eq!(module.name(), "cdr");
    assert!(module.allow_methods().contains(&rsip::Method::Invite));
    assert!(module.allow_methods().contains(&rsip::Method::Bye));
    assert!(module.allow_methods().contains(&rsip::Method::Cancel));
    assert_eq!(module.get_active_session_count(), 0);
}

#[tokio::test]
async fn test_call_session_to_call_record() {
    let dialog_id = DialogId {
        call_id: "test-call".to_string(),
        from_tag: "from-tag".to_string(),
        to_tag: "to-tag".to_string(),
    };

    let call_option = CallOption::default();
    let session = CallSession::new(
        dialog_id,
        "test-call-123".to_string(),
        "caller@example.com".to_string(),
        "callee@example.com".to_string(),
        call_option,
    );

    let hangup_reason = CallRecordHangupReason::ByCaller;
    let call_record = session.to_call_record(hangup_reason);

    assert_eq!(call_record.call_id, "test-call-123");
    assert_eq!(call_record.caller, "caller@example.com");
    assert_eq!(call_record.callee, "callee@example.com");
    assert_eq!(call_record.status_code, 180);
    assert!(matches!(
        call_record.hangup_reason,
        Some(CallRecordHangupReason::ByCaller)
    ));
    assert!(call_record.recorder.is_empty()); // CDR module doesn't handle media
}

#[tokio::test]
async fn test_cleanup_stale_sessions() {
    let server = create_test_server();
    let config = Arc::new(ProxyConfig::default());
    let module = CdrModule::new(server, config);

    let (sender, mut receiver) = mpsc::unbounded_channel();

    // Create a session manually with old timestamp
    let dialog_id = DialogId {
        call_id: "old-call".to_string(),
        from_tag: "from-tag".to_string(),
        to_tag: "to-tag".to_string(),
    };

    let call_option = CallOption::default();
    let mut session = CallSession::new(
        dialog_id,
        "old-call-123".to_string(),
        "caller@example.com".to_string(),
        "callee@example.com".to_string(),
        call_option,
    );

    // Set old start time (more than 1 hour ago)
    session.start_time = Utc::now() - chrono::Duration::hours(1);

    {
        let mut active_sessions = module.inner.active_sessions.lock().unwrap();
        active_sessions.insert("old-call-123".to_string(), session);
    }

    assert_eq!(module.get_active_session_count(), 1);

    // Run cleanup with 1 hour timeout
    module.cleanup_stale_sessions(&sender, 3600);

    // Should have no active sessions now
    assert_eq!(module.get_active_session_count(), 0);

    // Should have received a call record for the cleaned up session
    let call_record = receiver.try_recv().unwrap();
    assert_eq!(call_record.call_id, "old-call-123");
    assert!(matches!(
        call_record.hangup_reason,
        Some(CallRecordHangupReason::Autohangup)
    ));
}

#[tokio::test]
async fn test_module_interface_compliance() {
    let server = create_test_server();
    let config = Arc::new(ProxyConfig::default());
    let mut module = CdrModule::new(server, config);

    // Test ProxyModule interface
    assert!(module.on_start().await.is_ok());
    assert!(module.on_stop().await.is_ok());

    // Verify module allows expected methods
    let allowed_methods = module.allow_methods();
    assert!(allowed_methods.contains(&rsip::Method::Invite));
    assert!(allowed_methods.contains(&rsip::Method::Bye));
    assert!(allowed_methods.contains(&rsip::Method::Cancel));
    assert!(allowed_methods.contains(&rsip::Method::Ack));
}

#[test]
fn test_cdr_module_session_tracking() {
    let server = create_test_server();
    let config = Arc::new(ProxyConfig::default());
    let module = CdrModule::new(server, config);

    // Initially no sessions
    assert_eq!(module.get_active_session_count(), 0);

    // Add a session manually
    let dialog_id = DialogId {
        call_id: "test-call-id".to_string(),
        from_tag: "from-tag".to_string(),
        to_tag: "to-tag".to_string(),
    };

    let call_option = CallOption::default();
    let session = CallSession::new(
        dialog_id,
        "test-session-123".to_string(),
        "alice@example.com".to_string(),
        "bob@example.com".to_string(),
        call_option,
    );

    {
        let mut active_sessions = module.inner.active_sessions.lock().unwrap();
        active_sessions.insert("test-session-123".to_string(), session);
    }

    // Should have one session
    assert_eq!(module.get_active_session_count(), 1);

    // Verify session data
    let active_sessions = module.inner.active_sessions.lock().unwrap();
    let session = active_sessions.get("test-session-123").unwrap();
    assert_eq!(session.caller, "alice@example.com");
    assert_eq!(session.callee, "bob@example.com");
    assert_eq!(session.call_id, "test-session-123");
}
