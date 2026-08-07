#[cfg(test)]
mod tests {
    use crate::call::domain::{CallCommand, HangupCommand, LegId};
    use crate::call::runtime::SessionId;
    use crate::proxy::proxy_call::sip_session::SipSession;

    use crate::proxy::active_call_registry::{
        ActiveProxyCallEntry, ActiveProxyCallRegistry, ActiveProxyCallStatus,
    };
    use std::sync::Arc;

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

    #[tokio::test]
    async fn sip_session_with_registry() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());

        // Create a SipSession handle (lightweight for RWI)
        let session_id = SessionId::from("sip-test-session");
        let (handle, _cmd_rx) = SipSession::with_handle(session_id.clone());

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
}
