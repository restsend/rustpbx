use super::*;
use crate::call::app::{ControllerEvent, QueueDiagnosticRequest};
use crate::call::runtime::{AppResult, AppRuntime};
use crate::call::{DialDirection, Dialplan};
use crate::proxy::tests::common::create_test_request;
use crate::proxy::tests::test_sip_session_regressions::build_session_with_cmd_rx;

// Only the application identity is stubbed; commands run through the real session.
struct QueueRuntime;

#[async_trait::async_trait]
impl AppRuntime for QueueRuntime {
    async fn start_app(&self, _: &str, _: Option<serde_json::Value>, _: bool) -> AppResult<()> {
        Ok(())
    }
    async fn stop_app(&self, _: Option<String>) -> AppResult<()> {
        Ok(())
    }
    fn inject_event(&self, _: serde_json::Value) -> AppResult<()> {
        Ok(())
    }
    fn is_running(&self) -> bool {
        true
    }
    fn current_app(&self) -> Option<String> {
        Some("queue".into())
    }
    async fn current_app_invocation(&self) -> Option<crate::call::app::AppInvocationContext> {
        Some(crate::call::app::AppInvocationContext {
            app_execution_id: 1,
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn queue_recovery_session_rejects_transfer_active_leg_and_disconnected_caller() {
    for (transfer, caller_state, agent_state, reason) in [
        (true, LegState::Connected, None, "transfer_in_progress"),
        (false, LegState::Ended, None, "caller_not_connected"),
        (
            false,
            LegState::Connected,
            Some(LegState::Ringing),
            "active_agent_leg",
        ),
        (
            false,
            LegState::Connected,
            Some(LegState::Connected),
            "active_agent_leg",
        ),
    ] {
        let request = create_test_request(
            rsipstack::sip::Method::Invite,
            "caller",
            None,
            "example.test",
            None,
        );
        let dialplan = Dialplan::new(
            "queue-recovery-guards".into(),
            request,
            DialDirection::Inbound,
        );
        let (mut session, _, mut commands) = build_session_with_cmd_rx(dialplan).await;
        session.app_runtime = Arc::new(QueueRuntime);
        session.meta.transfer_in_progress = transfer;
        session.update_leg_state(&LegId::from("caller"), caller_state);
        if let Some(state) = agent_state {
            let mut leg = crate::call::domain::Leg::new(LegId::from("existing-agent"));
            leg.state = state;
            session
                .legs
                .add_leg(LegId::from("existing-agent"), leg, None);
        }
        let (events, mut event_rx) = mpsc::unbounded_channel();
        session.app_event_bridge.set_app_event_sender(Some(events));
        let (request, reply) =
            QueueDiagnosticRequest::new(true, "guard-test".into(), None, Duration::from_secs(1));
        session
            .execute_command(CallCommand::QueueDispatchDiagnostic { request }, None)
            .await;
        assert_eq!(reply.await.unwrap().unwrap_err(), reason);
        assert!(
            event_rx.try_recv().is_err(),
            "rejected command must not reach queue"
        );
        assert!(
            commands.try_recv().is_err(),
            "no dial/recording command may be emitted"
        );
        assert_eq!(
            session.legs.get(&LegId::from("caller")).unwrap().state,
            caller_state
        );
        if let Some(state) = agent_state {
            assert_eq!(
                session
                    .legs
                    .get(&LegId::from("existing-agent"))
                    .unwrap()
                    .state,
                state
            );
        }
    }
}

#[tokio::test]
async fn queue_recovery_read_only_query_does_not_block_sip_processing() {
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "caller",
        None,
        "example.test",
        None,
    );
    let dialplan = Dialplan::new("queue-read-only".into(), request, DialDirection::Inbound);
    let (mut session, _, mut commands) = build_session_with_cmd_rx(dialplan).await;
    session.app_runtime = Arc::new(QueueRuntime);
    let (events, mut event_rx) = mpsc::unbounded_channel();
    session.app_event_bridge.set_app_event_sender(Some(events));
    let (request, reply) =
        QueueDiagnosticRequest::new(false, "read-only".into(), None, Duration::from_secs(2));
    tokio::time::timeout(
        Duration::from_millis(100),
        session.execute_command(CallCommand::QueueDispatchDiagnostic { request }, None),
    )
    .await
    .expect("overview queries must not block SIP waiting for the queue actor");
    let Some(ControllerEvent::QueueDiagnostic(request)) = event_rx.recv().await else {
        panic!("expected typed query")
    };
    request.respond(Err("actor-test-response".into()));
    assert_eq!(reply.await.unwrap().unwrap_err(), "actor-test-response");
    assert!(commands.try_recv().is_err());
}

#[tokio::test]
async fn queue_recovery_guarded_dial_rechecks_lifecycle_and_generation_at_execution() {
    for (transfer, caller_state, generation, with_agent, reason) in [
        (true, LegState::Connected, 1, false, "transfer_in_progress"),
        (false, LegState::Ended, 1, false, "caller_not_connected"),
        (
            false,
            LegState::Connected,
            2,
            false,
            "queue_generation_changed",
        ),
        (false, LegState::Connected, 1, true, "active_agent_leg"),
    ] {
        let request = create_test_request(
            rsipstack::sip::Method::Invite,
            "caller",
            None,
            "example.test",
            None,
        );
        let plan = Dialplan::new(
            "guarded-recovery-dial".into(),
            request,
            DialDirection::Inbound,
        );
        let (mut session, _, mut commands) = build_session_with_cmd_rx(plan).await;
        session.app_runtime = Arc::new(QueueRuntime);
        session.update_leg_state(&LegId::from("caller"), caller_state);
        session.meta.transfer_in_progress = transfer;
        if with_agent {
            let mut agent = crate::call::domain::Leg::new(LegId::from("normal-agent"));
            agent.state = LegState::Connected;
            session
                .legs
                .add_leg(LegId::from("normal-agent"), agent, None);
        }
        let (events, mut event_rx) = mpsc::unbounded_channel();
        session.app_event_bridge.set_app_event_sender(Some(events));
        let wrapped = CallCommand::QueueRecoveryDial {
            request_id: "delayed-recovery".into(),
            app_execution_id: generation,
            deadline: tokio::time::Instant::now() + Duration::from_secs(1),
            command: Box::new(CallCommand::LegAdd {
                source_leg: None,
                target: "sip:1001@example.test".into(),
                leg_id: Some(LegId::from("recovery-agent")),
                headers: vec![],
                purpose: Some(crate::call::domain::LegPurpose::Agent),
            }),
        };
        let result = session
            .execute_command(
                CallCommand::FromApplication {
                    command: Box::new(wrapped),
                },
                None,
            )
            .await;
        assert!(!result.success, "{reason}");
        assert!(
            result
                .message
                .as_deref()
                .unwrap_or_default()
                .contains(reason)
        );
        let Some(ControllerEvent::Custom(name, data)) = event_rx.try_recv().ok() else {
            panic!("rejected recovery must acknowledge its exact pending dial to the queue");
        };
        assert_eq!(name, "queue_recovery_dial_failed");
        assert_eq!(data["request_id"], "delayed-recovery");
        assert_eq!(data["leg_id"], "recovery-agent");
        assert_eq!(data["reason"], reason);
        assert!(session.legs.get(&LegId::from("recovery-agent")).is_none());
        assert!(commands.try_recv().is_err());
        assert_eq!(
            session.legs.get(&LegId::from("caller")).unwrap().state,
            caller_state
        );
        if with_agent {
            assert_eq!(
                session
                    .legs
                    .get(&LegId::from("normal-agent"))
                    .unwrap()
                    .state,
                LegState::Connected
            );
        }
    }
}

#[tokio::test]
async fn queue_recovery_wrapper_rejects_arbitrary_command() {
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "caller",
        None,
        "example.test",
        None,
    );
    let plan = Dialplan::new("invalid-recovery".into(), request, DialDirection::Inbound);
    let (mut session, _, mut commands) = build_session_with_cmd_rx(plan).await;
    let wrapped = CallCommand::QueueRecoveryDial {
        request_id: "invalid".into(),
        app_execution_id: 1,
        deadline: tokio::time::Instant::now() + Duration::from_secs(1),
        command: Box::new(CallCommand::StopRecording),
    };
    let result = session.execute_command(wrapped, None).await;
    assert!(!result.success);
    assert_eq!(result.message.as_deref(), Some("invalid_recovery_command"));
    assert!(commands.try_recv().is_err());
}

#[tokio::test]
async fn queue_recovery_blank_placeholder_is_not_a_real_initializing_leg() {
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "caller",
        None,
        "example.test",
        None,
    );
    let plan = Dialplan::new("queue-placeholder".into(), request, DialDirection::Inbound);
    let (mut session, _, mut commands) = build_session_with_cmd_rx(plan).await;
    session.app_runtime = Arc::new(QueueRuntime);
    session.update_leg_state(&LegId::from("caller"), LegState::Connected);
    assert!(session.legs.get(&LegId::from("callee")).is_some());
    let (events, mut event_rx) = mpsc::unbounded_channel();
    session.app_event_bridge.set_app_event_sender(Some(events));
    let (request, reply) =
        QueueDiagnosticRequest::new(true, "placeholder".into(), None, Duration::from_secs(1));
    session
        .execute_command(CallCommand::QueueDispatchDiagnostic { request }, None)
        .await;
    let Some(ControllerEvent::QueueDiagnostic(request)) = event_rx.try_recv().ok() else {
        panic!("blank placeholder must allow owner-validated queue recovery")
    };
    assert_eq!(request.live_agent_legs, Some(0));
    request.respond(Err("forwarded".into()));
    assert_eq!(reply.await.unwrap().unwrap_err(), "forwarded");
    let id = LegId::from("real-initializing-agent");
    session
        .legs
        .add_leg(id.clone(), crate::call::domain::Leg::new(id.clone()), None);
    let (request, reply) = QueueDiagnosticRequest::new(
        true,
        "real-initializing".into(),
        None,
        Duration::from_secs(1),
    );
    session
        .execute_command(CallCommand::QueueDispatchDiagnostic { request }, None)
        .await;
    assert_eq!(reply.await.unwrap().unwrap_err(), "active_agent_leg");
    assert!(event_rx.try_recv().is_err());
    assert!(commands.try_recv().is_err());
    assert_eq!(session.legs.get(&id).unwrap().state, LegState::Initializing);
}

struct DelayedRecoveryLocator {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl crate::proxy::locator::Locator for DelayedRecoveryLocator {
    async fn register(
        &self,
        _: &str,
        _: Option<&str>,
        _: crate::call::Location,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn has_active_bindings(&self, _: &str, _: Option<&str>) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn unregister(&self, _: &str, _: Option<&str>) -> anyhow::Result<()> {
        Ok(())
    }
    async fn unregister_with_address(
        &self,
        _: &rsipstack::transport::SipAddr,
    ) -> anyhow::Result<Option<Vec<crate::call::Location>>> {
        Ok(None)
    }
    async fn lookup(
        &self,
        uri: &rsipstack::sip::Uri,
    ) -> anyhow::Result<Vec<crate::call::Location>> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(vec![crate::call::Location {
            aor: uri.clone(),
            ..Default::default()
        }])
    }
}

#[tokio::test]
async fn queue_recovery_deadline_cancels_locator_wait_before_creating_agent_leg() {
    for expired in [false, true] {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let locator = DelayedRecoveryLocator {
            entered: entered.clone(),
            release: release.clone(),
        };
        let (server, _) = crate::proxy::tests::common::create_test_server_with_config_and_locator(
            crate::config::ProxyConfig::default(),
            Arc::new(Box::new(locator)),
        )
        .await;
        // The shared server fixture uses a channel transport. Replace it with
        // real UDP so the positive control observes an actual wire INVITE.
        server
            .endpoint
            .inner
            .transport_layer
            .del_transport(&rsipstack::transport::SipAddr {
                r#type: Some(rsipstack::sip::Transport::Udp),
                addr: "127.0.0.1:5060".try_into().unwrap(),
            });
        let udp = rsipstack::transport::udp::UdpConnection::create_connection(
            "127.0.0.1:0".parse().unwrap(),
            None,
            None,
        )
        .await
        .unwrap();
        server
            .endpoint
            .inner
            .transport_layer
            .add_transport(udp.into());
        let endpoint = server.endpoint.inner.clone();
        let serving = tokio::spawn(async move {
            endpoint.serve().await.unwrap();
        });
        let phone = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target = format!("sip:1001@{}", phone.local_addr().unwrap());
        let request = create_test_request(
            rsipstack::sip::Method::Invite,
            "caller",
            None,
            "example.test",
            None,
        );
        let plan = Dialplan::new(
            "queue-recovery-deadline".into(),
            request,
            DialDirection::Inbound,
        );
        let (mut session, _, _commands) =
            crate::proxy::tests::test_sip_session_regressions::build_session_with_cmd_rx_on(
                server, plan,
            )
            .await;
        session.app_runtime = Arc::new(QueueRuntime);
        session.update_leg_state(&LegId::from("caller"), LegState::Connected);
        // A usable caller peer keeps the same locator path capable of real SIP origination.
        let caller = crate::media::leg::LegInner::new(
            "caller",
            &crate::media::leg::LegConfig::rtp_pcmu(),
            None,
        )
        .unwrap();
        session.legs.set_media_leg(&LegId::from("caller"), caller);
        let (events, mut event_rx) = mpsc::unbounded_channel();
        session.app_event_bridge.set_app_event_sender(Some(events));
        let deadline = tokio::time::Instant::now()
            + if expired {
                Duration::from_millis(40)
            } else {
                Duration::from_secs(2)
            };
        let command = CallCommand::QueueRecoveryDial {
            request_id: "locator-deadline".into(),
            app_execution_id: 1,
            deadline,
            command: Box::new(CallCommand::LegAdd {
                source_leg: None,
                target,
                leg_id: Some(LegId::from("deadline-agent")),
                headers: vec![],
                purpose: Some(crate::call::domain::LegPurpose::Agent),
            }),
        };
        let run = session.execute_command(command, None);
        let unblock = async {
            entered.notified().await;
            if expired {
                tokio::time::sleep_until(deadline + Duration::from_millis(20)).await;
            }
            release.notify_one();
        };
        let (result, ()) = tokio::join!(run, unblock);
        let mut packet = [0u8; 4096];
        if expired {
            assert!(!result.success);
            assert_eq!(result.message.as_deref(), Some("request_expired"));
            assert!(session.legs.get(&LegId::from("deadline-agent")).is_none());
            assert!(
                tokio::time::timeout(Duration::from_millis(80), phone.recv(&mut packet))
                    .await
                    .is_err(),
                "expired recovery must never send an INVITE"
            );
            let Some(ControllerEvent::Custom(name, data)) = event_rx.try_recv().ok() else {
                panic!("expired recovery must clear its pending queue attempt");
            };
            assert_eq!(name, "queue_recovery_dial_failed");
            assert_eq!(data["request_id"], "locator-deadline");
            assert_eq!(data["leg_id"], "deadline-agent");
        } else {
            assert!(
                result.success,
                "same locator path must really dial before expiry: {:?}",
                result.message
            );
            let size = tokio::time::timeout(Duration::from_secs(1), phone.recv(&mut packet))
                .await
                .unwrap()
                .unwrap();
            assert!(
                std::str::from_utf8(&packet[..size])
                    .unwrap()
                    .starts_with("INVITE ")
            );
            assert!(event_rx.try_recv().is_err());
            session.legs.remove(&LegId::from("deadline-agent"));
        }
        serving.abort();
    }
}
