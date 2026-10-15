use super::agent_registry::{AgentRegistry, MemoryRegistry, PresenceState};
use super::{
    CallApp, ControllerEvent, QueueDiagnosticRequest,
    queue::{QueueApp, QueueConfig},
    testing::MockCallStack,
};
use crate::call::domain::CallCommand;
use crate::call::{DialStrategy, Location, QueuePlan};
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn queue_recovery_missing_initial_kick_dials_once_and_preserves_entry() {
    // Tracing callsite interests are process-global. Isolate the actual-output
    // assertion from parallel tests that exercise the same sites without logging.
    const LOG_CHILD: &str = "RUSTPBX_QUEUE_RECOVERY_LOG_CHILD";
    if std::env::var_os(LOG_CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "call::app::queue_recovery_test::queue_recovery_missing_initial_kick_dials_once_and_preserves_entry",
                "--nocapture",
            ])
            .env(LOG_CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed;"),
            "{}{}",
            stdout,
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let plan = QueuePlan {
        hold: None,
        voice_prompts: Some(crate::call::VoicePrompts {
            transfer_prompt: None,
            service_prompt: Some("test-service.wav".into()),
            ..Default::default()
        }),
        dial_strategy: Some(DialStrategy::Sequential(vec![Location {
            aor: "sip:1001@example.test".parse().unwrap(),
            ..Default::default()
        }])),
        ..Default::default()
    };
    let registry = Arc::new(MemoryRegistry::new());
    registry
        .register(
            "1001".into(),
            "target".into(),
            "sip:1001@example.test".into(),
            vec![],
            1,
        )
        .await
        .unwrap();
    registry
        .register(
            "other".into(),
            "other".into(),
            "sip:other@example.test".into(),
            vec![],
            1,
        )
        .await
        .unwrap();
    registry
        .update_presence(
            "other",
            PresenceState::Ringing {
                call_id: Some("other-session".into()),
            },
        )
        .await
        .unwrap();
    let app: Box<dyn CallApp> = Box::new(
        QueueApp::new(plan, QueueConfig::default())
            .with_call_id("test-session".into())
            .with_agent_registry(registry.clone()),
    );
    let logs = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || RecoveryLogWriter(writer.clone()))
        .finish();
    let mut stack = tracing::subscriber::with_default(subscriber, || {
        MockCallStack::run(app, "caller", "queue")
    });
    assert!(
        stack.next_cmd(20).await.is_none(),
        "serial queue awaits initial kick"
    );
    let (request, reply) = queue_request(true, "retry-1".into(), None, Duration::from_secs(2));
    stack
        .event_sender()
        .send(ControllerEvent::QueueDiagnostic(request))
        .unwrap();
    let result = reply.await.unwrap().unwrap();
    assert_eq!(result.session_id, "test-session");
    assert_eq!(result.pending_dials, 1);
    assert_eq!(result.outcome, "dial_requested");
    let Some(CallCommand::QueueRecoveryDial { command, .. }) = stack.next_cmd(50).await else {
        panic!("expected guarded recovery dial");
    };
    let CallCommand::LegAdd {
        leg_id: Some(recovered_leg),
        ..
    } = *command
    else {
        panic!("expected agent LegAdd");
    };
    stack.custom("dial_next_agent", serde_json::json!({}));
    assert!(
        stack.next_cmd(20).await.is_none(),
        "a delayed original kick must not duplicate the recovery dial"
    );
    let (request, reply) = queue_request(true, "retry-1".into(), None, Duration::from_secs(2));
    stack
        .event_sender()
        .send(ControllerEvent::QueueDiagnostic(request))
        .unwrap();
    let repeated = reply.await.unwrap().unwrap();
    assert_eq!(repeated.outcome, "already_processed");
    assert!(
        stack.next_cmd(20).await.is_none(),
        "recovery must not duplicate INVITE"
    );
    let other = registry.get_agent("other").await.unwrap();
    assert_eq!(
        other.presence,
        PresenceState::Ringing {
            call_id: Some("other-session".into())
        }
    );
    assert_eq!(other.current_calls, 0);
    stack.custom(
        "agent_connected",
        serde_json::json!({
            "agent_uri": "sip:1001@example.test", "agent_id": "1001", "leg_id": recovered_leg.0,
        }),
    );
    let mut saw_service_prompt = false;
    while let Some(command) = stack.next_cmd(100).await {
        assert!(!matches!(
            command,
            CallCommand::LegAdd { .. } | CallCommand::QueueRecoveryDial { .. }
        ));
        if matches!(command, CallCommand::Play { .. }) {
            saw_service_prompt = true;
            break;
        }
    }
    assert!(saw_service_prompt);
    stack.custom("dial_next_agent", serde_json::json!({}));
    assert!(
        stack.next_cmd(20).await.is_none(),
        "late kick must not dial after agent connection"
    );
    let log = String::from_utf8(logs.lock().clone()).unwrap();
    assert!(log.contains("session_id=test-session"), "{log}");
    assert!(
        log.contains("targeted recovery requested agent dial"),
        "{log}"
    );
    assert!(log.contains("pending_dials=1"), "{log}");
    assert!(log.contains("retry_armed=false"), "{log}");
    assert!(log.contains("leg_id="), "{log}");
}

#[tokio::test]
async fn queue_recovery_wrong_group_and_expired_requests_do_not_dial() {
    let plan = QueuePlan {
        hold: None,
        dial_strategy: Some(DialStrategy::Sequential(vec![Location {
            aor: "sip:1001@example.test".parse().unwrap(),
            ..Default::default()
        }])),
        ..Default::default()
    };
    let app = QueueApp::new(plan, QueueConfig::default()).with_call_id("test-session".into());
    let mut stack = MockCallStack::run(Box::new(app), "caller", "queue");
    assert!(stack.next_cmd(20).await.is_none());
    let (request, reply) = queue_request(
        true,
        "wrong-group".into(),
        Some("other-group".into()),
        Duration::from_secs(2),
    );
    stack
        .event_sender()
        .send(ControllerEvent::QueueDiagnostic(request))
        .unwrap();
    let rejected = reply.await.unwrap().unwrap();
    assert_eq!(rejected.reason, "skill_group_changed");
    assert_eq!(rejected.outcome, "rejected");
    let (request, reply) = queue_request(true, "expired".into(), None, Duration::ZERO);
    stack
        .event_sender()
        .send(ControllerEvent::QueueDiagnostic(request))
        .unwrap();
    assert!(reply.await.unwrap().is_err());
    assert!(stack.next_cmd(20).await.is_none());
    let (request, reply) = queue_request(false, "read-only".into(), None, Duration::from_secs(2));
    stack
        .event_sender()
        .send(ControllerEvent::QueueDiagnostic(request))
        .unwrap();
    let observed = reply.await.unwrap().unwrap();
    assert!(observed.recoverable);
    assert_eq!(observed.pending_dials, 0);
    assert!(stack.next_cmd(20).await.is_none());
}

#[tokio::test]
async fn queue_recovery_healthy_wait_is_read_only_and_keeps_retry() {
    let registry = Arc::new(MemoryRegistry::new());
    let app = QueueApp::new(
        QueuePlan {
            hold: None,
            ..Default::default()
        },
        QueueConfig {
            skill_group: Some("group-a".into()),
            retry_interval_secs: 60,
            ..Default::default()
        },
    )
    .with_call_id("test-session".into())
    .with_agent_registry(registry);
    let mut stack = MockCallStack::run(Box::new(app), "caller", "queue");
    assert!(matches!(
        stack.next_cmd(100).await,
        Some(CallCommand::Answer { .. })
    ));
    let (request, reply) = queue_request(
        true,
        "healthy".into(),
        Some("group-a".into()),
        Duration::from_secs(2),
    );
    stack
        .event_sender()
        .send(ControllerEvent::QueueDiagnostic(request))
        .unwrap();
    let result = reply.await.unwrap().unwrap();
    assert_eq!(result.reason, "healthy_waiting");
    assert!(!result.recoverable);
    assert!(result.retry_armed);
    assert!(stack.next_cmd(20).await.is_none());
}

#[derive(Clone)]
struct RecoveryLogWriter(Arc<parking_lot::Mutex<Vec<u8>>>);

impl std::io::Write for RecoveryLogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn queue_recovery_retry_observation_includes_fired_event_until_actor_consumes_it() {
    let (commands, _command_rx) = tokio::sync::mpsc::channel(16);
    let handle = crate::proxy::proxy_call::sip_session::SipSessionHandle::new_for_test(
        "timer-session",
        commands,
    );
    let (_events, events_rx) = tokio::sync::mpsc::unbounded_channel();
    let (ctrl, mut timers) = super::CallController::new(handle, events_rx);
    ctrl.set_timeout("queue_retry", Duration::ZERO);
    let timer_id = tokio::time::timeout(Duration::from_secs(1), timers.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(timer_id, "queue_retry");
    assert!(
        ctrl.timeout_is_armed("queue_retry"),
        "a fired retry still queued for the actor is not missing"
    );
    ctrl.set_timeout("queue_retry", Duration::ZERO);
    assert_eq!(timers.recv().await.as_deref(), Some("queue_retry"));
    ctrl.record_timer_consumed("queue_retry");
    assert!(
        ctrl.timeout_is_armed("queue_retry"),
        "second same-name event remains queued"
    );
    ctrl.set_timeout("queue_retry", Duration::from_secs(60));
    ctrl.record_timer_consumed("queue_retry");
    assert!(
        ctrl.timeout_is_armed("queue_retry"),
        "consuming older delivery cannot remove newer task"
    );
    ctrl.cancel_timeout("queue_retry");
    assert!(!ctrl.timeout_is_armed("queue_retry"));
}

struct DelayedRecoveryRegistry {
    inner: MemoryRegistry,
    reserved: tokio::sync::Notify,
    unblock: tokio::sync::Notify,
    no_answers: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl AgentRegistry for DelayedRecoveryRegistry {
    async fn register(
        &self,
        id: String,
        name: String,
        uri: String,
        skills: Vec<String>,
        capacity: u32,
    ) -> anyhow::Result<()> {
        self.inner.register(id, name, uri, skills, capacity).await
    }
    async fn unregister(&self, id: &str) -> anyhow::Result<()> {
        self.inner.unregister(id).await
    }
    async fn get_agent(&self, id: &str) -> Option<super::agent_registry::AgentRecord> {
        self.inner.get_agent(id).await
    }
    async fn list_agents(&self) -> Vec<super::agent_registry::AgentRecord> {
        self.inner.list_agents().await
    }
    async fn update_presence(&self, id: &str, state: PresenceState) -> anyhow::Result<()> {
        self.inner.update_presence(id, state).await
    }
    async fn find_available_agents(
        &self,
        skills: &[String],
    ) -> Vec<super::agent_registry::AgentRecord> {
        self.inner.find_available_agents(skills).await
    }
    async fn select_agent(
        &self,
        skills: &[String],
        strategy: super::agent_registry::RoutingStrategy,
    ) -> Option<super::agent_registry::AgentRecord> {
        self.inner.select_agent(skills, strategy).await
    }
    async fn resolve_target(&self, target: &str) -> Vec<String> {
        self.inner.resolve_target(target).await
    }
    async fn note_agent_no_answer(&self, agent_id: &str, call_id: &str) {
        self.no_answers
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.note_agent_no_answer(agent_id, call_id).await;
    }
    async fn fifo_begin_dispatch(&self, call_id: &str) {
        // Model a committed reservation followed by an uncontrollable DB/enrichment wait.
        self.inner
            .update_presence(
                "1001",
                PresenceState::Ringing {
                    call_id: Some(call_id.into()),
                },
            )
            .await
            .unwrap();
        self.reserved.notify_one();
        self.unblock.notified().await;
    }
    async fn agents_ringing_for_call(&self, call_id: &str) -> Vec<String> {
        self.inner.list_agents().await.into_iter().filter_map(|agent| {
            matches!(agent.presence, PresenceState::Ringing { call_id: Some(ref id) } if id == call_id).then_some(agent.agent_id)
        }).collect()
    }
    async fn release_call(&self, agent_id: &str, call_id: &str) -> bool {
        self.inner.release_call(agent_id, call_id).await
    }
    async fn release_unanswered_reservation(&self, agent_id: &str, call_id: &str) -> bool {
        self.inner
            .release_unanswered_reservation(agent_id, call_id)
            .await
    }
}

#[tokio::test]
async fn queue_recovery_cancellation_releases_only_own_untracked_reservation() {
    let registry = Arc::new(DelayedRecoveryRegistry {
        inner: MemoryRegistry::new(),
        reserved: tokio::sync::Notify::new(),
        unblock: tokio::sync::Notify::new(),
        no_answers: std::sync::atomic::AtomicUsize::new(0),
    });
    registry
        .register(
            "1001".into(),
            "target".into(),
            "sip:1001@example.test".into(),
            vec![],
            1,
        )
        .await
        .unwrap();
    registry
        .register(
            "other".into(),
            "other".into(),
            "sip:other@example.test".into(),
            vec![],
            1,
        )
        .await
        .unwrap();
    registry
        .update_presence(
            "other",
            PresenceState::Ringing {
                call_id: Some("other-session".into()),
            },
        )
        .await
        .unwrap();
    let plan = QueuePlan {
        hold: None,
        dial_strategy: Some(DialStrategy::Sequential(vec![Location {
            aor: "sip:1001@example.test".parse().unwrap(),
            ..Default::default()
        }])),
        ..Default::default()
    };
    let app = QueueApp::new(plan, QueueConfig::default())
        .with_call_id("test-session".into())
        .with_agent_registry(registry.clone());
    let mut stack = MockCallStack::run(Box::new(app), "caller", "queue");
    assert!(stack.next_cmd(20).await.is_none());
    let (request, reply) =
        queue_request(true, "cancel-recovery".into(), None, Duration::from_secs(2));
    stack
        .event_sender()
        .send(ControllerEvent::QueueDiagnostic(request))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), registry.reserved.notified())
        .await
        .unwrap();
    stack.cancel();
    let joined = tokio::time::timeout(Duration::from_millis(200), stack.join()).await;
    registry.unblock.notify_one();
    assert!(
        joined.is_ok(),
        "caller cancellation must interrupt delayed recovery promptly"
    );
    joined.unwrap().unwrap();
    assert!(reply.await.unwrap().is_err());
    assert_eq!(
        registry.get_agent("1001").await.unwrap().presence,
        PresenceState::Idle
    );
    assert_eq!(
        registry.get_agent("other").await.unwrap().presence,
        PresenceState::Ringing {
            call_id: Some("other-session".into())
        }
    );
}

fn queue_request(
    recover: bool,
    request_id: String,
    group: Option<String>,
    timeout: Duration,
) -> (
    QueueDiagnosticRequest,
    tokio::sync::oneshot::Receiver<Result<super::QueueDispatchDiagnostic, String>>,
) {
    let (mut request, reply) = QueueDiagnosticRequest::new(recover, request_id, group, timeout);
    // The isolated actor harness represents an owner that has just validated this live caller.
    request.expected_app_execution_id = Some(1);
    request.live_agent_legs = Some(0);
    request.live_transfer_in_progress = Some(false);
    request.live_caller_connected = Some(true);
    request.live_session_observed_at_ms = Some(chrono::Utc::now().timestamp_millis());
    (request, reply)
}

#[tokio::test]
async fn queue_recovery_reservation_cleanup_never_releases_busy_or_another_call() {
    let registry = MemoryRegistry::new();
    registry
        .register(
            "agent".into(),
            "agent".into(),
            "sip:agent@example.test".into(),
            vec![],
            1,
        )
        .await
        .unwrap();
    for state in [
        PresenceState::Busy {
            call_id: Some("target-session".into()),
        },
        PresenceState::Ringing {
            call_id: Some("other-session".into()),
        },
    ] {
        registry
            .update_presence("agent", state.clone())
            .await
            .unwrap();
        assert!(
            !registry
                .release_unanswered_reservation("agent", "target-session")
                .await
        );
        assert_eq!(registry.get_agent("agent").await.unwrap().presence, state);
    }
    registry
        .update_presence(
            "agent",
            PresenceState::Ringing {
                call_id: Some("target-session".into()),
            },
        )
        .await
        .unwrap();
    assert!(
        registry
            .release_unanswered_reservation("agent", "target-session")
            .await
    );
    assert_eq!(
        registry.get_agent("agent").await.unwrap().presence,
        PresenceState::Idle
    );
}

#[tokio::test]
async fn queue_recovery_prompt_backpressure_keeps_already_queued_dial_reservation() {
    let registry = Arc::new(DelayedRecoveryRegistry {
        inner: MemoryRegistry::new(),
        reserved: tokio::sync::Notify::new(),
        unblock: tokio::sync::Notify::new(),
        no_answers: std::sync::atomic::AtomicUsize::new(0),
    });
    registry
        .register(
            "1001".into(),
            "agent".into(),
            "sip:1001@example.test".into(),
            vec![],
            1,
        )
        .await
        .unwrap();
    registry.unblock.notify_one();
    let plan = QueuePlan {
        hold: None,
        voice_prompts: Some(crate::call::VoicePrompts {
            transfer_prompt: Some("transfer.wav".into()),
            ..Default::default()
        }),
        dial_strategy: Some(DialStrategy::Sequential(vec![Location {
            aor: "sip:1001@example.test".parse().unwrap(),
            ..Default::default()
        }])),
        ..Default::default()
    };
    let app = QueueApp::new(plan, QueueConfig::default())
        .with_call_id("test-session".into())
        .with_agent_registry(registry.clone());
    // PinAgentMeta + Trace + guarded LegAdd fill this channel. The later Play fails.
    let (commands, mut command_rx) = tokio::sync::mpsc::channel(3);
    let handle = crate::proxy::proxy_call::sip_session::SipSessionHandle::new_for_test(
        "test-session",
        commands,
    );
    let (events, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let (controller, timers) = super::CallController::new(handle, event_rx);
    let context = super::ApplicationContext::new(
        sea_orm::DatabaseConnection::default(),
        super::CallInfo {
            session_id: "test-session".into(),
            caller: "caller".into(),
            callee: "queue".into(),
            direction: "inbound".into(),
            started_at: chrono::Utc::now(),
            sip_headers: Default::default(),
            route_name: None,
        },
        Arc::new(crate::config::Config::default()),
        reqwest::Client::new(),
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    let actor = tokio::spawn(
        super::AppEventLoop::new(
            Box::new(app),
            controller,
            context,
            cancel.child_token(),
            timers,
        )
        .run(),
    );
    let (request, reply) = queue_request(
        true,
        "prompt-backpressure".into(),
        None,
        Duration::from_secs(2),
    );
    events
        .send(ControllerEvent::QueueDiagnostic(request))
        .unwrap();
    assert!(
        reply.await.unwrap().is_err(),
        "the post-dial Play must hit channel backpressure"
    );
    let mut guarded_dials = 0;
    while let Ok(command) = command_rx.try_recv() {
        if matches!(command, CallCommand::QueueRecoveryDial { .. }) {
            guarded_dials += 1;
        }
    }
    let presence_after_prompt = registry.get_agent("1001").await.unwrap().presence;
    cancel.cancel();
    actor.await.unwrap().unwrap();
    assert_eq!(
        guarded_dials, 1,
        "the dial was already committed to the session channel"
    );
    assert_eq!(
        presence_after_prompt,
        PresenceState::Ringing {
            call_id: Some("test-session".into())
        },
        "a prompt failure cannot free an in-flight agent dial"
    );
}

#[tokio::test]
async fn queue_recovery_rejection_matches_request_and_leg_then_retries_without_no_answer() {
    for (busy_on_other_call, skill_group) in [
        (false, Some("group-a")),
        (true, Some("group-a")),
        (false, None),
    ] {
        let registry = Arc::new(DelayedRecoveryRegistry {
            inner: MemoryRegistry::new(),
            reserved: tokio::sync::Notify::new(),
            unblock: tokio::sync::Notify::new(),
            no_answers: std::sync::atomic::AtomicUsize::new(0),
        });
        registry.unblock.notify_one();
        registry
            .register(
                "1001".into(),
                "agent".into(),
                "sip:1001@example.test".into(),
                vec![],
                1,
            )
            .await
            .unwrap();
        registry
            .update_presence(
                "1001",
                PresenceState::Ringing {
                    call_id: Some("test-session".into()),
                },
            )
            .await
            .unwrap();
        let app = QueueApp::new(
            QueuePlan {
                hold: None,
                dial_strategy: Some(DialStrategy::Sequential(vec![Location {
                    aor: "sip:1001@example.test".parse().unwrap(),
                    ..Default::default()
                }])),
                ..Default::default()
            },
            QueueConfig {
                skill_group: skill_group.map(str::to_string),
                retry_interval_secs: 60,
                ..Default::default()
            },
        )
        .with_call_id("test-session".into())
        .with_agent_registry(registry.clone());
        let mut stack = MockCallStack::run(Box::new(app), "caller", "queue");
        assert!(stack.next_cmd(20).await.is_none());
        let (request, reply) =
            queue_request(true, "rejected-dial".into(), None, Duration::from_secs(2));
        stack
            .event_sender()
            .send(ControllerEvent::QueueDiagnostic(request))
            .unwrap();
        assert_eq!(reply.await.unwrap().unwrap().pending_dials, 1);
        let Some(CallCommand::QueueRecoveryDial { command, .. }) = stack.next_cmd(50).await else {
            panic!("expected recovery dial");
        };
        let CallCommand::LegAdd {
            leg_id: Some(leg), ..
        } = *command
        else {
            panic!("expected agent leg");
        };
        for (request_id, leg_id) in [
            ("stale-request", leg.0.as_str()),
            ("rejected-dial", "stale-leg"),
        ] {
            stack.custom("queue_recovery_dial_failed", serde_json::json!({"request_id":request_id,"leg_id":leg_id,"reason":"request_expired"}));
            let (request, reply) =
                queue_request(false, "observe-stale".into(), None, Duration::from_secs(2));
            stack
                .event_sender()
                .send(ControllerEvent::QueueDiagnostic(request))
                .unwrap();
            assert_eq!(reply.await.unwrap().unwrap().pending_dials, 1);
        }
        if busy_on_other_call {
            registry
                .update_presence(
                    "1001",
                    PresenceState::Busy {
                        call_id: Some("other-session".into()),
                    },
                )
                .await
                .unwrap();
        }
        stack.custom("queue_recovery_dial_failed", serde_json::json!({"request_id":"rejected-dial","leg_id":leg.0,"reason":"request_expired"}));
        // A timer already delivered to the actor can outlive cancel_timeout.
        stack.timeout("agent_ring_timeout");
        let (request, reply) = queue_request(
            false,
            "observe-rejected".into(),
            None,
            Duration::from_secs(2),
        );
        stack
            .event_sender()
            .send(ControllerEvent::QueueDiagnostic(request))
            .unwrap();
        let evidence = reply.await.unwrap().unwrap();
        assert_eq!(
            evidence.pending_dials, 0,
            "a rejected SIP command is not a ringing attempt"
        );
        assert!(evidence.retry_armed);
        let expected = if busy_on_other_call {
            PresenceState::Busy {
                call_id: Some("other-session".into()),
            }
        } else {
            PresenceState::Idle
        };
        assert_eq!(registry.get_agent("1001").await.unwrap().presence, expected);
        assert_eq!(
            registry
                .no_answers
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        while let Some(command) = stack.next_cmd(20).await {
            assert!(
                !matches!(
                    command,
                    CallCommand::LegRemove { .. } | CallCommand::QueueRecoveryDial { .. }
                ),
                "rejection must not synthesize a ring timeout or redial immediately"
            );
            if let CallCommand::InjectAppEvent { event } = command {
                assert!(!serde_json::to_string(&event).unwrap().contains("no_answer"));
            }
        }
        if skill_group.is_none() {
            registry.unblock.notify_one();
            stack.timeout("queue_retry");
            assert!(
                matches!(stack.next_cmd(100).await, Some(CallCommand::LegAdd { .. })),
                "a rejected static queue dial must resume its normal retry"
            );
        }
    }
}
