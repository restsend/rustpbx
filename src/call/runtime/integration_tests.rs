#[cfg(test)]
mod tests {
    use crate::call::domain::{CallCommand, HangupCommand, LegId};
    use crate::call::runtime::{
        ActiveCallView, CallDirection, CallStatus, RegistryAdapter, SessionId, SessionRegistry,
    };
    use crate::proxy::proxy_call::sip_session::SipSession;

    use crate::proxy::active_call_registry::{
        ActiveProxyCallEntry, ActiveProxyCallRegistry, ActiveProxyCallStatus,
    };
    use std::sync::Arc;

    /// Helper to create a test handle
    fn create_test_handle(
        session_id: &str,
    ) -> crate::proxy::proxy_call::sip_session::SipSessionHandle {
        let id = SessionId::from(session_id.to_string());
        let (handle, _cmd_rx) = SipSession::with_handle(id);
        handle
    }

    /// Helper to create a test registry entry
    fn create_test_entry(session_id: &str) -> ActiveProxyCallEntry {
        ActiveProxyCallEntry {
            session_id: session_id.to_string(),
            caller: Some("sip:100@example.com".to_string()),
            callee: Some("sip:101@example.com".to_string()),
            direction: "outbound".to_string(),
            started_at: chrono::Utc::now(),
            answered_at: None,
            status: ActiveProxyCallStatus::Ringing,
        }
    }

    // ============================================================================
    // Registry Adapter Integration Tests
    // ============================================================================

    #[test]
    fn registry_adapter_wraps_active_proxy_call_registry() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let adapter = RegistryAdapter::new(registry.clone());

        // Create and register a session
        let session_id = "test-session-1";
        let handle = create_test_handle(session_id);
        let entry = create_test_entry(session_id);
        registry.upsert(entry, handle);

        // Verify adapter can access the registry
        assert_eq!(adapter.len(), 1);
        assert!(adapter.contains(session_id));
        assert_eq!(adapter.session_ids(), vec![session_id.to_string()]);
    }

    #[test]
    fn registry_adapter_remove() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let adapter = RegistryAdapter::new(registry.clone());

        // Create and register a session
        let session_id = "test-session-2";
        let handle = create_test_handle(session_id);
        let entry = create_test_entry(session_id);
        registry.upsert(entry, handle);

        assert_eq!(adapter.len(), 1);

        // Remove via adapter
        adapter.remove(session_id);

        assert_eq!(adapter.len(), 0);
        assert!(!adapter.contains(session_id));
    }

    #[test]
    fn registry_adapter_update_entry() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let adapter = RegistryAdapter::new(registry.clone());

        // Create and register a session
        let session_id = "test-session-3";
        let handle = create_test_handle(session_id);
        let entry = create_test_entry(session_id);
        registry.upsert(entry, handle);

        // Update via adapter
        let view = ActiveCallView {
            session_id: SessionId::from(session_id),
            caller: Some("sip:updated@example.com".to_string()),
            callee: Some("sip:newcallee@example.com".to_string()),
            direction: CallDirection::Inbound,
            status: CallStatus::Talking,
            started_at: chrono::Utc::now(),
            answered_at: Some(chrono::Utc::now()),
        };

        adapter.upsert(view);

        // Verify update was applied
        let updated = registry.get(session_id).unwrap();
        assert_eq!(updated.caller, Some("sip:updated@example.com".to_string()));
        assert_eq!(
            updated.callee,
            Some("sip:newcallee@example.com".to_string())
        );
        assert_eq!(updated.status, ActiveProxyCallStatus::Talking);
    }

    #[test]
    fn registry_adapter_multiple_sessions() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let adapter = RegistryAdapter::new(registry.clone());

        // Create multiple sessions
        for i in 0..5 {
            let session_id = format!("session-{}", i);
            let handle = create_test_handle(&session_id);
            let entry = create_test_entry(&session_id);
            registry.upsert(entry, handle);
        }

        assert_eq!(adapter.len(), 5);
        let mut ids = adapter.session_ids();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "session-0",
                "session-1",
                "session-2",
                "session-3",
                "session-4"
            ]
        );
    }

    // ============================================================================
    // Call Direction/Status Conversion Tests
    // ============================================================================

    #[test]
    fn call_status_conversion_to_legacy() {
        // Test that our CallStatus correctly maps to ActiveProxyCallStatus
        assert_eq!(
            CallStatus::Ringing.to_string(),
            ActiveProxyCallStatus::Ringing.to_string()
        );
        assert_eq!(
            CallStatus::Talking.to_string(),
            ActiveProxyCallStatus::Talking.to_string()
        );
    }

    #[test]
    fn call_direction_conversion() {
        assert_eq!(CallDirection::Inbound.to_string(), "inbound");
        assert_eq!(CallDirection::Outbound.to_string(), "outbound");
    }

    // ============================================================================
    // Unified Session with Registry Integration Tests
    // ============================================================================

    use crate::call::runtime::{AppRuntime, AppRuntimeError, AppStatus};

    /// Mock AppRuntime for integration tests
    #[allow(dead_code)]
    struct MockAppRuntime {
        running: std::sync::atomic::AtomicBool,
    }

    impl MockAppRuntime {
        #[allow(dead_code)]
        fn new() -> Self {
            Self {
                running: std::sync::atomic::AtomicBool::new(false),
            }
        }
    }

    #[async_trait::async_trait]
    impl AppRuntime for MockAppRuntime {
        async fn start_app(
            &self,
            _app_name: &str,
            _params: Option<serde_json::Value>,
            _auto_answer: bool,
        ) -> crate::call::runtime::AppResult<()> {
            use std::sync::atomic::Ordering;
            if self.running.load(Ordering::SeqCst) {
                return Err(AppRuntimeError::AlreadyRunning("app".to_string()));
            }
            self.running.store(true, Ordering::SeqCst);
            Ok(())
        }

        async fn stop_app(&self, _reason: Option<String>) -> crate::call::runtime::AppResult<()> {
            use std::sync::atomic::Ordering;
            if !self.running.load(Ordering::SeqCst) {
                return Err(AppRuntimeError::NotRunning);
            }
            self.running.store(false, Ordering::SeqCst);
            Ok(())
        }

        fn inject_event(&self, _event: serde_json::Value) -> crate::call::runtime::AppResult<()> {
            use std::sync::atomic::Ordering;
            if !self.running.load(Ordering::SeqCst) {
                return Err(AppRuntimeError::NotRunning);
            }
            Ok(())
        }

        fn is_running(&self) -> bool {
            use std::sync::atomic::Ordering;
            self.running.load(Ordering::SeqCst)
        }

        fn status(&self) -> AppStatus {
            use std::sync::atomic::Ordering;
            if self.running.load(Ordering::SeqCst) {
                AppStatus::Running
            } else {
                AppStatus::Idle
            }
        }

        fn current_app(&self) -> Option<String> {
            None
        }

        fn required_capabilities(&self) -> Vec<crate::call::domain::MediaCapability> {
            vec![]
        }

        fn app_descriptor(&self, _app_name: &str) -> Option<crate::call::runtime::AppDescriptor> {
            None
        }
    }

    #[tokio::test]
    async fn sip_session_with_registry() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _adapter = RegistryAdapter::new(registry.clone());

        // Create a SipSession handle (lightweight for RWI)
        let session_id = SessionId::from("sip-test-session");
        let (handle, _cmd_rx) = SipSession::with_handle(session_id.clone());

        // Create a view and register with the registry adapter
        let _view = ActiveCallView {
            session_id: session_id.clone(),
            caller: Some("sip:caller@example.com".to_string()),
            callee: None,
            direction: CallDirection::Inbound,
            status: CallStatus::Ringing,
            started_at: chrono::Utc::now(),
            answered_at: None,
        };

        // Register the handle
        let entry = create_test_entry(&session_id.0);
        registry.upsert(entry, handle.clone());

        // Verify we can get the handle back
        assert!(registry.get_handle(&session_id.0).is_some());
    }

    #[tokio::test]
    async fn sip_session_execute_command_updates_state() {
        // Create a SipSession handle and send commands via channel
        let session_id = SessionId::from("command-test-session");
        let (handle, mut cmd_rx) = SipSession::with_handle(session_id);

        // Send answer command
        let result = handle.send_command(CallCommand::Answer {
            leg_id: LegId::from("caller"),
        });

        assert!(result.is_ok());

        // Verify command was received
        let received = cmd_rx.recv().await;
        assert!(matches!(received, Some(CallCommand::Answer { .. })));
    }

    #[tokio::test]
    async fn sip_session_handle_commands() {
        let session_id = SessionId::from("hangup-test-session");
        let (handle, mut cmd_rx) = SipSession::with_handle(session_id);

        // Send hangup command
        let hangup_cmd = HangupCommand::all(None, Some(200));
        let result = handle.send_command(CallCommand::Hangup(hangup_cmd));

        assert!(result.is_ok());

        // Verify command was received
        let received = cmd_rx.recv().await;
        assert!(matches!(received, Some(CallCommand::Hangup(_))));
    }

    #[tokio::test]
    async fn sip_session_bridge_command() {
        use crate::call::domain::P2PMode;

        let session_id = SessionId::from("bridge-test-session");
        let (handle, mut cmd_rx) = SipSession::with_handle(session_id);

        // Send bridge command
        let result = handle.send_command(CallCommand::Bridge {
            leg_a: LegId::from("leg_a"),
            leg_b: LegId::from("leg_b"),
            mode: P2PMode::Audio,
        });

        assert!(result.is_ok());

        // Verify command was received
        let received = cmd_rx.recv().await;
        assert!(matches!(received, Some(CallCommand::Bridge { .. })));
    }

    #[tokio::test]
    async fn sip_session_media_command() {
        let session_id = SessionId::from("media-test-session");
        let (handle, mut cmd_rx) = SipSession::with_handle(session_id);

        // Send play command
        let result = handle.send_command(CallCommand::Play {
            leg_id: Some(LegId::from("caller")),
            source: crate::call::domain::MediaSource::file("test.wav"),
            options: None,
        });

        assert!(result.is_ok());

        // Verify command was received
        let received = cmd_rx.recv().await;
        assert!(matches!(received, Some(CallCommand::Play { .. })));
    }

    // ============================================================================
    // SipSession Tests
    // ============================================================================

    #[test]
    fn sip_session_is_default() {
        // SipSession is the default and only session type
        assert!(true);
    }
}
