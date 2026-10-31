//! Step IVR executor test suite (moved verbatim from executor.rs).
//! Sibling module so the 7k-line suite does not bloat the implementation
//! file; repo convention is `<module>_test.rs`.

#![cfg(test)]

use super::config::{ActionNode, EntryAction};
use super::provider::{
    ActionProvider, ProviderContext, ProviderEvent, SessionContext, SessionEndReason,
    SessionEndTag,
};
use super::trace::IvrTraceEntry;
use crate::call::app::CallApp;
use std::time::Duration;
use super::executor::{
    IVR_END_REASON_KEY, IVR_LAST_ERROR_KEY, IVR_REENTRY_STATE_KEY, IVR_STATUS_KEY,
    IVR_SUSPENDED_EXECUTION_ID_KEY,
};
use super::executor::*;
use crate::call::app::ivr::{ResumeEventMode, RetryConfig, StepProvider};
use crate::call::app::testing::MockCallStack;
use crate::call::app::{ApplicationContext, CallInfo};
use crate::call::domain::CallCommand;
use crate::config::Config;
use async_trait::async_trait;
use chrono::Utc;
use sea_orm::DatabaseConnection;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

/// A mock provider that returns pre-defined nodes in sequence
struct MockProvider {
    nodes: Vec<ActionNode>,
    idx: std::sync::Mutex<usize>,
    start_called: std::sync::Mutex<bool>,
    start_context: std::sync::Mutex<Option<SessionContext>>,
    end_called: std::sync::Mutex<bool>,
    events: std::sync::Mutex<Vec<Option<ProviderEvent>>>,
    contexts: std::sync::Mutex<Vec<ProviderContext>>,
}

impl MockProvider {
    fn new(nodes: Vec<ActionNode>) -> Self {
        Self {
            nodes,
            idx: std::sync::Mutex::new(0),
            start_called: std::sync::Mutex::new(false),
            start_context: std::sync::Mutex::new(None),
            end_called: std::sync::Mutex::new(false),
            events: std::sync::Mutex::new(Vec::new()),
            contexts: std::sync::Mutex::new(Vec::new()),
        }
    }
}

struct MockProviderHandle(Arc<MockProvider>);

#[async_trait]
impl ActionProvider for MockProvider {
    async fn next_action(&self, ctx: ProviderContext) -> anyhow::Result<ActionNode> {
        self.events.lock().unwrap().push(ctx.event.clone());
        self.contexts.lock().unwrap().push(ctx);
        let mut idx = self.idx.lock().unwrap();
        if *idx < self.nodes.len() {
            let node = self.nodes[*idx].clone();
            *idx += 1;
            Ok(node)
        } else {
            Err(anyhow::anyhow!("no more nodes"))
        }
    }

    async fn on_session_start(&self, ctx: &SessionContext) -> anyhow::Result<()> {
        *self.start_called.lock().unwrap() = true;
        *self.start_context.lock().unwrap() = Some(ctx.clone());
        Ok(())
    }

    async fn on_session_end(
        &self,
        _reason: &SessionEndReason,
        _session_id: &str,
    ) -> anyhow::Result<()> {
        *self.end_called.lock().unwrap() = true;
        Ok(())
    }
}

#[async_trait]
impl ActionProvider for MockProviderHandle {
    async fn next_action(&self, ctx: ProviderContext) -> anyhow::Result<ActionNode> {
        self.0.next_action(ctx).await
    }

    async fn on_session_start(&self, ctx: &SessionContext) -> anyhow::Result<()> {
        self.0.on_session_start(ctx).await
    }

    async fn on_session_end(
        &self,
        reason: &SessionEndReason,
        session_id: &str,
    ) -> anyhow::Result<()> {
        self.0.on_session_end(reason, session_id).await
    }
}

fn mock_app(nodes: Vec<ActionNode>) -> StepIvrApp {
    StepIvrApp::with_provider(Box::new(MockProvider::new(nodes)))
}

fn make_test_context() -> ApplicationContext {
    ApplicationContext::new(
        DatabaseConnection::default(),
        CallInfo {
            session_id: "test-session".into(),
            caller: "1001".into(),
            callee: "2000".into(),
            direction: "inbound".into(),
            started_at: Utc::now(),
            sip_headers: HashMap::new(),
            route_name: None,
        },
        Arc::new(Config::default()),
        reqwest::Client::new(),
    )
}

/// Consumer contract for `ivr_step_trace`: `duration_ms` must equal
/// `step_end_time - step_start_time`. The executor measures with a
/// monotonic Instant while the stamps are wall clock — both captured at
/// the same execution points, so ±25ms absorbs scheduling jitter.
/// Returns the asserted duration for further window assertions.
fn assert_trace_duration_matches_stamps(payload: &serde_json::Value) -> u64 {
    let start = payload["step_start_time"]
        .as_str()
        .expect("step_start_time must be present");
    let end = payload["step_end_time"]
        .as_str()
        .expect("step_end_time must be present");
    let expected = crate::call::app::ivr::trace::duration_ms_between(Some(start), end);
    let duration = payload["duration_ms"]
        .as_u64()
        .unwrap_or_else(|| panic!("duration_ms must be present, got {payload}"));
    assert!(
        (duration as i64 - expected as i64).abs() <= 25,
        "duration_ms ({duration}ms) must equal step_end_time - step_start_time \
         ({expected}ms, ±25ms scheduling tolerance)"
    );
    duration
}

fn parse_stamp(payload: &serde_json::Value, field: &str) -> chrono::DateTime<chrono::Utc> {
    payload[field]
        .as_str()
        .unwrap_or_else(|| panic!("{field} must be present"))
        .parse::<chrono::DateTime<chrono::Utc>>()
        .expect("trace stamps must be RFC3339")
}

#[tokio::test]
async fn ivr_exec_step_provider_uses_invocation_identity_and_variables() {
    let provider = Arc::new(MockProvider::new(vec![ActionNode::new(
        EntryAction::Transfer {
            target: "2001".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        },
    )]));
    let mut context = make_test_context();
    context.invocation = Some(crate::call::app::AppInvocationContext {
        app_execution_id: 2,
        callee: "39230".into(),
        sip_headers: HashMap::from([("X-Business-Type".into(), "34".into())]),
        variables: HashMap::from([
            ("session_id".into(), "business-value".into()),
            ("order_id".into(), "order-001".into()),
        ]),
    });
    let app = StepIvrApp::with_provider(Box::new(MockProviderHandle(provider.clone())))
        .with_name("routed-menu")
        .with_ivr_params(serde_json::json!({
            "workflow_id": "workflow-test",
            "subject_id": "subject-test",
            "operation": "confirm",
            "source": "source-test"
        }));
    let mut stack = MockCallStack::run_with_context(Box::new(app), context);

    stack
        .assert_cmd(2000, "accept", |command| {
            matches!(command, CallCommand::Answer { .. })
        })
        .await;
    stack
        .assert_cmd(
            200,
            "transfer",
            |command| matches!(command, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    let start = provider.start_context.lock().unwrap().clone().unwrap();
    assert_eq!(start.session_id, "test-session");
    assert_eq!(start.app_execution_id, 2);
    assert_eq!(start.callee, "39230");
    assert_eq!(start.sip_headers.as_ref().unwrap()["X-Business-Type"], "34");
    assert_eq!(start.variables["session_id"], "business-value");
    assert_eq!(start.variables["ivr_name"], "routed-menu");
    assert_eq!(start.variables["workflow_id"], "workflow-test");
    assert_eq!(start.variables["subject_id"], "subject-test");
    assert_eq!(start.variables["operation"], "confirm");
    assert_eq!(start.variables["source"], "source-test");
    let contexts = provider.contexts.lock().unwrap();
    assert_eq!(contexts[0].session_id, "test-session");
    assert_eq!(contexts[0].app_execution_id, 2);
    assert_eq!(contexts[0].variables["session_id"], "business-value");
    assert_eq!(contexts[0].variables["ivr_name"], "routed-menu");
    assert_eq!(contexts[0].variables["operation"], "confirm");
}

#[tokio::test]
async fn ivr_exec_returns_exit_result_variables() {
    let mut node = ActionNode::new(EntryAction::Exit);
    node.result_variables = HashMap::from([
        ("decision".into(), "accepted".into()),
        ("payload".into(), "result-value".into()),
        ("result_kind".into(), "post_flow".into()),
    ]);
    let mut context = make_test_context();
    context.invocation = Some(crate::call::app::AppInvocationContext {
        app_execution_id: 2,
        ..Default::default()
    });
    let extensions = context.session_extensions.clone();
    extensions
        .write()
        .insert(crate::proxy::proxy_call::ivr_exec_hook::IvrExecState {
            app_execution_id: Some(2),
            request_id: "request-test".into(),
            held_leg: None,
            initiator_leg: crate::call::domain::LegId::from("callee"),
            webhook_url: None,
            app_name: "ivr".into(),
            metadata: serde_json::Value::Null,
        });
    let app = StepIvrApp::with_provider(Box::new(MockProvider::new(vec![node])));
    let mut stack = MockCallStack::run_with_context(Box::new(app), context);

    stack
        .assert_cmd(2000, "accept", |command| {
            matches!(command, CallCommand::Answer { .. })
        })
        .await;
    stack.join().await.expect("exit flow must complete");

    let guard = extensions.read();
    let result = guard
        .get::<crate::proxy::proxy_call::ivr_exec_hook::IvrExecResult>()
        .expect("IVR result must be available for the completion hook");
    assert_eq!(result.status, "completed");
    assert_eq!(result.reason, "normal");
    assert_eq!(result.collected["decision"], "accepted");
    assert_eq!(result.collected["payload"], "result-value");
    assert_eq!(result.collected["result_kind"], "post_flow");
}

struct BlockingProvider {
    entered_next: Arc<Notify>,
    release_next: Arc<Notify>,
    end_called: Arc<AtomicBool>,
}

#[async_trait]
impl ActionProvider for BlockingProvider {
    async fn next_action(&self, _ctx: ProviderContext) -> anyhow::Result<ActionNode> {
        self.entered_next.notify_one();
        self.release_next.notified().await;
        Ok(ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        }))
    }

    async fn on_session_end(
        &self,
        _reason: &SessionEndReason,
        _session_id: &str,
    ) -> anyhow::Result<()> {
        self.end_called.store(true, Ordering::SeqCst);
        Ok(())
    }
}

struct FailingProvider;

#[async_trait]
impl ActionProvider for FailingProvider {
    async fn next_action(&self, _ctx: ProviderContext) -> anyhow::Result<ActionNode> {
        Err(anyhow::anyhow!("provider bootstrap failed"))
    }
}

#[tokio::test]
async fn test_transfer() {
    let mut stack = MockCallStack::run(
        Box::new(mock_app(vec![ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        })])),
        "1001",
        "2000",
    );
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;
}

#[tokio::test]
async fn non_await_transfer_rejects_headers_instead_of_silently_dropping_them() {
    let mut stack = MockCallStack::run(
        Box::new(mock_app(vec![ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            headers: HashMap::from([("X-Route-Metadata".into(), "workflow=example".into())]),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        })])),
        "1001",
        "2000",
    );
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "reject-transfer-headers", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path },
                    ..
                } if path == "sounds/error.wav"
            )
        })
        .await;
    stack.cancel();
    stack.join().await.expect("cancel should stop app");
}

#[tokio::test]
async fn transfer_result_finalizes_pending_trace_with_original_trigger() {
    use crate::call::app::ControllerEvent;
    use crate::call::domain::TransferOutcome;
    use crate::rwi::gateway::RwiGateway;

    let mut transfer = ActionNode::new(EntryAction::Transfer {
        target: "2001".into(),
        headers: HashMap::new(),
        params: HashMap::new(),
        return_app: None,
        return_target: None,
    });
    transfer.wait_for_result = true;
    if let EntryAction::Transfer { headers, .. } = &mut transfer.action {
        headers.insert("X-Route-Metadata".into(), "workflow=example".into());
    }
    transfer.step_id = Some("transfer-step".into());
    transfer.extra = Some(serde_json::json!({"nodetype": "transfer"}));
    let hangup = ActionNode::new(EntryAction::hangup_none());
    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let mut app = mock_app(vec![transfer, hangup]);
    app.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gateway)));

    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "transfer", |c| {
            matches!(c, CallCommand::TransferAwaitResult { target, headers, .. }
                if target == "2001"
                    && headers.get("X-Route-Metadata").map(String::as_str)
                        == Some("workflow=example"))
        })
        .await;
    stack
        .event_sender()
        .send(ControllerEvent::TransferResult(
            TransferOutcome::NotConnected,
        ))
        .unwrap();
    stack
        .assert_cmd(2000, "hangup", |c| matches!(c, CallCommand::Hangup { .. }))
        .await;

    // Single finalized trace: original trigger, end time filled, no
    // wait_finalized duplicate.
    let finalized = events.try_recv().expect("transfer trace must be enqueued");
    assert_eq!(finalized.event.payload["step_id"], "transfer-step");
    assert_eq!(finalized.event.payload["trigger"]["type"], "session_start");
    assert!(finalized.event.payload["step_end_time"].is_string());
    // Subsequent events are legitimate (hangup step trace, session_end) —
    // none may be a wait_finalized duplicate.
    while let Ok(ev) = events.try_recv() {
        assert_ne!(
            ev.event.payload["trigger"]["type"], "wait_finalized",
            "wait completion must not emit a wait_finalized duplicate"
        );
    }
}

#[tokio::test]
async fn test_prompt_then_transfer_via_next() {
    let node = ActionNode::with_next(
        EntryAction::Prompt {
            file: Some("hello.wav".into()),
            tts_text: None,
            tts_voice: None,
            record_name_list: None,
            interruptible: false,
            tts_api_url: None,

            delay_before_ms: 0,
            delay_after_ms: 0,
        },
        ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        }),
    );

    let mut stack = MockCallStack::run(Box::new(mock_app(vec![node])), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path }, ..
                } if path == "hello.wav"
            )
        })
        .await;

    stack.audio_complete("ivr_prompt");

    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;
}

#[tokio::test]
async fn test_prompt_then_provider() {
    use crate::rwi::gateway::RwiGateway;

    let mut prompt = ActionNode::new(EntryAction::Prompt {
        file: Some("hello.wav".into()),
        tts_text: None,
        tts_voice: None,
        record_name_list: None,
        interruptible: false,
        tts_api_url: None,

        delay_before_ms: 0,
        delay_after_ms: 0,
    });
    prompt.step_id = Some("prompt-step".into());
    let mut transfer = ActionNode::new(EntryAction::Transfer {
        target: "2001".into(),
        headers: HashMap::new(),
        params: HashMap::new(),
        return_app: None,
        return_target: None,
    });
    transfer.step_id = Some("transfer-step".into());
    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let mut app = mock_app(vec![prompt, transfer]);
    app.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gateway)));

    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path }, ..
                } if path == "hello.wav"
            )
        })
        .await;
    stack.audio_complete("ivr_prompt");

    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    let prompt_trace = events.try_recv().expect("prompt trace must be enqueued");
    let transfer_trace = events.try_recv().expect("transfer trace must be enqueued");
    assert_eq!(prompt_trace.event.payload["step_id"], "prompt-step");
    assert_eq!(
        prompt_trace.event.payload["trigger"]["type"],
        "session_start"
    );
    assert_eq!(transfer_trace.event.payload["step_id"], "transfer-step");
}

// ── Step duration contract: duration_ms == step_end_time - step_start_time ──
//
// 播报+等待输入 nodes (prompt playback, menu key wait) are what consumers
// build nodeDuration analytics from. The executor measures these waits
// with a monotonic Instant; the emitted event must carry a duration that
// reflects the real wait window AND agrees with its own RFC3339 stamps.

/// A prompt step waiting for playback completion must report the real
/// wait window — not 0 — and the duration must equal its own stamps.
#[tokio::test]
async fn test_prompt_wait_duration_matches_stamps() {
    use crate::rwi::gateway::RwiGateway;

    let mut prompt = ActionNode::new(EntryAction::Prompt {
        file: Some("hello.wav".into()),
        tts_text: None,
        tts_voice: None,
        record_name_list: None,
        interruptible: false,
        tts_api_url: None,

        delay_before_ms: 0,
        delay_after_ms: 0,
    });
    prompt.step_id = Some("prompt-step".into());
    let transfer = ActionNode::new(EntryAction::Transfer {
        target: "2001".into(),
        headers: HashMap::new(),
        params: HashMap::new(),
        return_app: None,
        return_target: None,
    });

    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let mut app = mock_app(vec![prompt, transfer]);
    app.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gateway)));

    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path },
                    ..
                } if path == "hello.wav"
            )
        })
        .await;

    // Simulated playback + listener dwell: the WaitFor prompt step must
    // measure this window (finalize happens when audio_complete arrives).
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    stack.audio_complete("ivr_prompt");

    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    let prompt_trace = events.try_recv().expect("prompt trace must be enqueued");
    let payload = &prompt_trace.event.payload;
    assert_eq!(payload["step_id"], "prompt-step");
    let duration = assert_trace_duration_matches_stamps(payload);
    assert!(
        duration >= 50,
        "prompt wait duration must reflect the real wait window (~80ms), got {duration}ms"
    );
}

/// A menu step waiting for a key press (greeting dwell + input window)
/// must report the accumulated wait — not 0 — matching its own stamps.
#[tokio::test]
async fn test_menu_key_wait_duration_matches_stamps() {
    use crate::rwi::gateway::RwiGateway;

    let mut menu = ActionNode::new(EntryAction::DtmfMenu {
        greeting: Some("menu.wav".into()),
        greeting_text: None,
        greeting_record_list: None,
        greeting_voice: None,
        timeout_ms: 5000,
        max_retries: 3,
        entries: HashMap::new(),
        timeout_action: None,
        invalid_action: None,
        greeting_api_url: None,
    });
    menu.step_id = Some("menu-step".into());
    let transfer = ActionNode::new(EntryAction::Transfer {
        target: "2001".into(),
        headers: HashMap::new(),
        params: HashMap::new(),
        return_app: None,
        return_target: None,
    });

    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let mut app = mock_app(vec![menu, transfer]);
    app.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gateway)));

    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path },
                    ..
                } if path == "menu.wav"
            )
        })
        .await;

    // Greeting dwell + key-wait window — the menu step must measure it all.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    stack.audio_complete("ivr_menu_greeting");
    let _ = stack.drain_cmds();
    stack.dtmf("1");

    // DTMF barge-in stops playback before the next step's transfer.
    stack
        .assert_cmd(2000, "stop", |c| {
            matches!(c, CallCommand::StopPlayback { .. })
        })
        .await;
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    let menu_trace = events.try_recv().expect("menu trace must be enqueued");
    let payload = &menu_trace.event.payload;
    assert_eq!(payload["step_id"], "menu-step");
    assert_eq!(payload["trigger"]["type"], "dtmf");
    assert_eq!(payload["trigger"]["detail"]["digit"], "1");
    let duration = assert_trace_duration_matches_stamps(payload);
    assert!(
        duration >= 50,
        "menu key-wait duration must reflect the real wait window (~80ms), got {duration}ms"
    );
}

/// A chained node (`node.next` — skips `request_next`) must re-stamp its
/// own start; inheriting the previous step's start would inflate its
/// duration by the whole previous step's window.
#[tokio::test]
async fn test_chained_node_gets_fresh_step_start() {
    use crate::rwi::gateway::RwiGateway;

    let mut node_a = ActionNode::with_next(
        EntryAction::Prompt {
            file: Some("hello.wav".into()),
            tts_text: None,
            tts_voice: None,
            record_name_list: None,
            interruptible: false,
            tts_api_url: None,

            delay_before_ms: 0,
            delay_after_ms: 0,
        },
        {
            let mut b = ActionNode::new(EntryAction::Prompt {
                file: Some("world.wav".into()),
                tts_text: None,
                tts_voice: None,
                record_name_list: None,
                interruptible: false,
                tts_api_url: None,

                delay_before_ms: 0,
                delay_after_ms: 0,
            });
            b.step_id = Some("step-b".into());
            b
        },
    );
    node_a.step_id = Some("step-a".into());
    let transfer = ActionNode::new(EntryAction::Transfer {
        target: "2001".into(),
        headers: HashMap::new(),
        params: HashMap::new(),
        return_app: None,
        return_target: None,
    });

    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let mut app = mock_app(vec![node_a, transfer]);
    app.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gateway)));

    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path },
                    ..
                } if path == "hello.wav"
            )
        })
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    stack.audio_complete("ivr_prompt");

    // Chained step B executes without a provider round-trip.
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path },
                    ..
                } if path == "world.wav"
            )
        })
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    stack.audio_complete("ivr_prompt");

    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    let trace_a = events.try_recv().expect("step A trace must be enqueued");
    let trace_b = events
        .try_recv()
        .expect("chained step B trace must be enqueued");
    let _transfer_trace = events.try_recv().expect("transfer trace must be enqueued");

    assert_eq!(trace_a.event.payload["step_id"], "step-a");
    assert_eq!(trace_b.event.payload["step_id"], "step-b");

    let dur_a = assert_trace_duration_matches_stamps(&trace_a.event.payload);
    let dur_b = assert_trace_duration_matches_stamps(&trace_b.event.payload);
    assert!(
        dur_a >= 40,
        "step A duration must reflect its own window (~60ms), got {dur_a}ms"
    );
    assert!(
        dur_b >= 40,
        "chained step B duration must reflect its own window (~60ms), got {dur_b}ms"
    );

    let start_a = parse_stamp(&trace_a.event.payload, "step_start_time");
    let start_b = parse_stamp(&trace_b.event.payload, "step_start_time");
    let gap = (start_b - start_a).num_milliseconds();
    assert!(
        gap >= 40,
        "chained node must re-stamp its own start (gap {gap}ms) — \
         inheriting step A's start inflates step B's duration"
    );
}

#[tokio::test]
async fn test_empty_prompt_completes_without_audio() {
    use crate::rwi::gateway::RwiGateway;

    let prompt: ActionNode = serde_json::from_value(serde_json::json!({
        "type": "prompt",
        "tts_text": "",
        "step_id": "empty-prompt-step",
        "extra": { "nodetype": "dynamic_prompt" }
    }))
    .expect("provider-shaped empty Prompt must deserialize");
    let mut transfer = ActionNode::new(EntryAction::Transfer {
        target: "2001".into(),
        headers: HashMap::new(),
        params: HashMap::new(),
        return_app: None,
        return_target: None,
    });
    transfer.step_id = Some("next-step".into());
    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let mut app = mock_app(vec![prompt, transfer]);
    app.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gateway)));

    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    let prompt_trace = events
        .try_recv()
        .expect("empty prompt trace must be enqueued");
    let transfer_trace = events.try_recv().expect("next step trace must be enqueued");
    assert_eq!(prompt_trace.event.payload["step_id"], "empty-prompt-step");
    assert_eq!(prompt_trace.event.payload["action_type"], "Prompt");
    assert_eq!(
        prompt_trace.event.payload["extra"]["nodetype"],
        "dynamic_prompt"
    );
    assert_eq!(transfer_trace.event.payload["step_id"], "next-step");
    assert_eq!(
        transfer_trace.event.payload["trigger"]["type"],
        "audio_complete"
    );
}

#[tokio::test]
async fn test_missing_prompt_audio_reports_provider_error() {
    let prompt: ActionNode = serde_json::from_value(serde_json::json!({
        "type": "prompt",
        "step_id": "missing-audio-step"
    }))
    .expect("provider-shaped Prompt without media must deserialize");
    let mut transfer = ActionNode::new(EntryAction::Transfer {
        target: "2001".into(),
        headers: HashMap::new(),
        params: HashMap::new(),
        return_app: None,
        return_target: None,
    });
    transfer.step_id = Some("error-step".into());
    let provider = Arc::new(MockProvider::new(vec![prompt, transfer]));
    let app = StepIvrApp::with_provider(Box::new(MockProviderHandle(provider.clone())));

    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    assert!(provider.events.lock().unwrap().iter().any(|event| {
        matches!(
            event,
            Some(ProviderEvent::Error { reason }) if reason == "TTS service not available"
        )
    }));
}

#[tokio::test]
async fn test_dtmf_menu_with_local_entries() {
    let mut entries = HashMap::new();
    entries.insert(
        "1".into(),
        ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        }),
    );
    entries.insert(
        "2".into(),
        ActionNode::new(EntryAction::Queue {
            target: "support".into(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        }),
    );

    let menu = ActionNode::new(EntryAction::DtmfMenu {
        greeting: Some("menu.wav".into()),
        greeting_text: None,
        greeting_record_list: None,
        greeting_voice: None,
        timeout_ms: 5000,
        max_retries: 3,
        entries,
        timeout_action: Some(Box::new(ActionNode::new(EntryAction::Repeat))),
        invalid_action: Some(Box::new(ActionNode::new(EntryAction::Repeat))),
        greeting_api_url: None,
    });

    let mut stack = MockCallStack::run(Box::new(mock_app(vec![menu])), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path }, ..
                } if path == "menu.wav"
            )
        })
        .await;
    stack.audio_complete("ivr_menu_greeting");

    // Drain pending cmds before injecting DTMF
    let _ = stack.drain_cmds();
    std::thread::sleep(std::time::Duration::from_millis(50));
    stack.dtmf("1");

    // DTMF triggers StopPlayback first, then Transfer
    stack
        .assert_cmd(2000, "stop", |c| {
            matches!(c, CallCommand::StopPlayback { .. })
        })
        .await;
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;
}

// ── Provider-driven menu (empty entries): DTMF must be forwarded to provider ──

// ── Capturing provider scaffold ──
// One ActionProvider test double shared by the provider-driven tests:
// captures every ProviderEvent and delegates the action decision to a
// `respond` closure. Replaces the per-test provider structs that only
// differed in their event→action table.

type CapturedEvents = Arc<std::sync::Mutex<Vec<Option<ProviderEvent>>>>;

/// The `DtmfMenu` node most provider tests return (fixed greeting file,
/// empty entries, 5s timeout, 3 retries).
fn test_menu_node(greeting_text: Option<&str>) -> ActionNode {
    ActionNode::new(EntryAction::DtmfMenu {
        greeting: Some("menu.wav".into()),
        greeting_text: greeting_text.map(Into::into),
        greeting_record_list: None,
        greeting_voice: None,
        timeout_ms: 5000,
        max_retries: 3,
        entries: HashMap::new(),
        timeout_action: None,
        invalid_action: None,
        greeting_api_url: None,
    })
}

/// The `Transfer → 2001` node used as the scripted terminal action.
fn test_transfer_node() -> ActionNode {
    ActionNode::new(EntryAction::Transfer {
        target: "2001".into(),
        headers: HashMap::new(),
        params: HashMap::new(),
        return_app: None,
        return_target: None,
    })
}

struct CapturingProvider<F> {
    captured_events: CapturedEvents,
    /// Whether the FIRST `next_action` call is recorded too (the
    /// menu-first variant returns its scripted menu unrecorded, mirroring
    /// the original EventCapturingProvider semantics).
    capture_first: bool,
    calls: std::sync::atomic::AtomicUsize,
    respond: F,
}

impl<F> CapturingProvider<F>
where
    F: Fn(&ProviderContext, bool) -> anyhow::Result<ActionNode> + Send + Sync,
{
    /// Capture every event; the action comes from `respond`
    /// (`first` = whether this is the first `next_action` call).
    fn scripted(respond: F) -> (Self, CapturedEvents) {
        Self::build(true, respond)
    }

    fn build(capture_first: bool, respond: F) -> (Self, CapturedEvents) {
        let captured_events: CapturedEvents = Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            Self {
                captured_events: captured_events.clone(),
                capture_first,
                calls: std::sync::atomic::AtomicUsize::new(0),
                respond,
            },
            captured_events,
        )
    }
}

/// First call returns `menu(None)` unrecorded; later calls capture the
/// event and transfer to 2001. (The original EventCapturingProvider.)
fn capturing_menu_first() -> (
    CapturingProvider<
        impl Fn(&ProviderContext, bool) -> anyhow::Result<ActionNode> + Send + Sync,
    >,
    CapturedEvents,
) {
    CapturingProvider::build(false, |_, first| {
        if first {
            return Ok(test_menu_node(None));
        }
        Ok(test_transfer_node())
    })
}

#[async_trait]
impl<F> ActionProvider for CapturingProvider<F>
where
    F: Fn(&ProviderContext, bool) -> anyhow::Result<ActionNode> + Send + Sync,
{
    async fn next_action(&self, ctx: ProviderContext) -> anyhow::Result<ActionNode> {
        let first = self
            .calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 0;
        if self.capture_first || !first {
            self.captured_events.lock().unwrap().push(ctx.event.clone());
        }
        (self.respond)(&ctx, first)
    }
}

#[tokio::test]
async fn test_provider_driven_menu_dtmf_forwards_digit() {
    let (provider, events_handle) = capturing_menu_first();
    let app = StepIvrApp::with_provider(Box::new(provider)).with_name("provider-driven-ivr");
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path },
                    ..
                } if path == "menu.wav"
            )
        })
        .await;

    stack.audio_complete("ivr_menu_greeting");
    let _ = stack.drain_cmds();
    std::thread::sleep(std::time::Duration::from_millis(50));
    stack.dtmf("1");

    stack
        .assert_cmd(2000, "stop", |c| {
            matches!(c, CallCommand::StopPlayback { .. })
        })
        .await;
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    let events: Vec<String> = events_handle
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            Some(ProviderEvent::Dtmf { digit }) => Some(format!("dtmf:{digit}")),
            Some(ProviderEvent::SessionStart) => Some("session_start".into()),
            Some(ProviderEvent::AudioComplete { .. }) => Some("audio_complete".into()),
            Some(other) => Some(format!("{:?}", other)),
            None => Some("none".into()),
        })
        .collect();
    assert!(
        events.iter().any(|e| e == "dtmf:1"),
        "provider should have received Dtmf{{digit:\"1\"}}, got: {:?}",
        events
    );
}

#[tokio::test]
async fn test_provider_driven_menu_tts_dtmf_forwards_digit() {
    let (provider, captured) = CapturingProvider::scripted(|ctx, _| {
        if matches!(&ctx.event, Some(ProviderEvent::Dtmf { .. })) {
            Ok(test_transfer_node())
        } else {
            Ok(test_menu_node(Some("请按1转坐席")))
        }
    });
    let app = StepIvrApp::with_provider(Box::new(provider)).with_name("menu-tts-ivr");
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path },
                    ..
                } if path == "menu.wav"
            )
        })
        .await;

    stack.audio_complete("ivr_menu_greeting");
    let _ = stack.drain_cmds();
    std::thread::sleep(std::time::Duration::from_millis(50));
    stack.dtmf("1");

    stack
        .assert_cmd(2000, "stop", |c| {
            matches!(c, CallCommand::StopPlayback { .. })
        })
        .await;
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    let events: Vec<String> = captured
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            Some(ProviderEvent::Dtmf { digit }) => Some(format!("dtmf:{digit}")),
            Some(ProviderEvent::SessionStart) => Some("session_start".into()),
            Some(other) => Some(format!("{other:?}")),
            None => Some("none".into()),
        })
        .collect();
    assert!(
        events.iter().any(|e| e == "dtmf:1"),
        "greeting_text DtmfMenu must POST digit to provider, got: {:?}",
        events
    );
}

#[tokio::test]
async fn test_menu_tts_dtmf_during_greeting_forwards_digit() {
    let (provider, captured) = CapturingProvider::scripted(|ctx, _| {
        if matches!(&ctx.event, Some(ProviderEvent::Dtmf { .. })) {
            Ok(test_transfer_node())
        } else {
            Ok(test_menu_node(Some("请按1转坐席")))
        }
    });
    let app = StepIvrApp::with_provider(Box::new(provider)).with_name("menu-tts-bargein");
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path },
                    ..
                } if path == "menu.wav"
            )
        })
        .await;

    // Barge-in while the TTS/file greeting is still playing.
    let _ = stack.drain_cmds();
    std::thread::sleep(std::time::Duration::from_millis(50));
    stack.dtmf("2");

    stack
        .assert_cmd(2000, "stop", |c| {
            matches!(c, CallCommand::StopPlayback { .. })
        })
        .await;
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    let events: Vec<String> = captured
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            Some(ProviderEvent::Dtmf { digit }) => Some(format!("dtmf:{digit}")),
            _ => None,
        })
        .collect();
    assert!(
        events.iter().any(|e| e == "dtmf:2"),
        "DTMF during menu_tts greeting must be pushed to provider, got: {:?}",
        events
    );
}

#[tokio::test]
async fn test_buffered_dtmf_flushed_after_menu_greeting() {
    let (provider, captured) = CapturingProvider::scripted(|ctx, _| {
        if matches!(&ctx.event, Some(ProviderEvent::Dtmf { .. })) {
            return Ok(test_transfer_node());
        }
        let menu = test_menu_node(Some("请按1"));
        Ok(ActionNode::with_next(
            EntryAction::Prompt {
                file: Some("announce.wav".into()),
                tts_text: None,
                tts_voice: None,
                record_name_list: None,
                interruptible: false,
                tts_api_url: None,

                delay_before_ms: 0,
                delay_after_ms: 0,
            },
            menu,
        ))
    });
    let app = StepIvrApp::with_provider(Box::new(provider)).with_name("flush-dtmf-ivr");
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path },
                    ..
                } if path == "announce.wav"
            )
        })
        .await;

    // Digit during the non-interruptible announcement is buffered.
    stack.dtmf("1");
    stack.audio_complete("ivr_prompt");

    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path },
                    ..
                } if path == "menu.wav"
            )
        })
        .await;

    // Greeting complete must flush the buffered digit to the provider.
    stack.audio_complete("ivr_menu_greeting");
    stack
        .assert_cmd(2000, "stop", |c| {
            matches!(c, CallCommand::StopPlayback { .. })
        })
        .await;
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    let events: Vec<String> = captured
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            Some(ProviderEvent::Dtmf { digit }) => Some(format!("dtmf:{digit}")),
            Some(ProviderEvent::SessionStart) => Some("session_start".into()),
            Some(other) => Some(format!("{other:?}")),
            None => Some("none".into()),
        })
        .collect();
    assert!(
        events.iter().any(|e| e == "dtmf:1"),
        "buffered DTMF must be delivered after menu greeting, got: {:?}",
        events
    );
}

// ── Verify trace trigger for local-menu DTMF ──

#[tokio::test]
async fn test_local_menu_dtmf_trace_trigger() {
    use crate::call::app::ivr::trace::IvrTraceCollector;

    let trace = IvrTraceCollector::new();

    let mut entries = HashMap::new();
    entries.insert(
        "1".into(),
        ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        }),
    );

    let menu = ActionNode::new(EntryAction::DtmfMenu {
        greeting: Some("menu.wav".into()),
        greeting_text: None,
        greeting_record_list: None,
        greeting_voice: None,
        timeout_ms: 5000,
        max_retries: 3,
        entries,
        timeout_action: None,
        invalid_action: None,
        greeting_api_url: None,
    });

    let mut app: StepIvrApp = mock_app(vec![menu]);
    app.trace = Some(trace.clone());
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path },
                    ..
                } if path == "menu.wav"
            )
        })
        .await;
    stack.audio_complete("ivr_menu_greeting");
    let _ = stack.drain_cmds();
    std::thread::sleep(std::time::Duration::from_millis(50));
    stack.dtmf("1");

    stack
        .assert_cmd(2000, "stop", |c| {
            matches!(c, CallCommand::StopPlayback { .. })
        })
        .await;
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let sessions = trace.sessions().await;
    let sess = &sessions[0];
    let entries = trace.query_by_session(&sess.session_id).await;
    let menu_entry = entries.iter().find(|e| e.action_type == "DtmfMenu");
    assert!(
        menu_entry.is_some(),
        "expected a DtmfMenu trace entry, all entries: {:?}",
        entries
            .iter()
            .map(|e| (&e.action_type, e.trigger.type_name()))
            .collect::<Vec<_>>()
    );
    let menu_entry = menu_entry.unwrap();
    assert_eq!(
        menu_entry.trigger.type_name(), "dtmf",
        "local DtmfMenu step trigger should be 'dtmf' (unified), got: {:?}",
        menu_entry.trigger
    );
    assert_eq!(
        menu_entry
            .trigger.digit(),
        Some("1"),
        "DtmfMenu step trigger should contain digit '1'"
    );
}

// ── Verify trace trigger for provider-driven menu DTMF ──

#[tokio::test]
async fn test_provider_driven_menu_dtmf_trace_trigger() {
    use crate::call::app::ivr::trace::IvrTraceCollector;

    let trace = IvrTraceCollector::new();
    let (provider, _events_handle) = capturing_menu_first();
    let mut app: StepIvrApp =
        StepIvrApp::with_provider(Box::new(provider)).with_name("provider-driven-trace-ivr");
    app.trace = Some(trace.clone());
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path },
                    ..
                } if path == "menu.wav"
            )
        })
        .await;
    stack.audio_complete("ivr_menu_greeting");
    let _ = stack.drain_cmds();
    std::thread::sleep(std::time::Duration::from_millis(50));
    stack.dtmf("1");

    stack
        .assert_cmd(2000, "stop", |c| {
            matches!(c, CallCommand::StopPlayback { .. })
        })
        .await;
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let sessions = trace.sessions().await;
    let sess = &sessions[0];
    let entries = trace.query_by_session(&sess.session_id).await;
    let menu_entry = entries.iter().find(|e| e.action_type == "DtmfMenu");
    assert!(
        menu_entry.is_some(),
        "expected a DtmfMenu trace entry, all entries: {:?}",
        entries
            .iter()
            .map(|e| (&e.action_type, e.trigger.type_name()))
            .collect::<Vec<_>>()
    );
    let menu_entry = menu_entry.unwrap();
    assert_eq!(
        menu_entry.trigger.type_name(), "dtmf",
        "provider-driven DtmfMenu step trigger should be 'dtmf' (after fix), got: {:?}",
        menu_entry.trigger
    );
    assert_eq!(
        menu_entry
            .trigger.digit(),
        Some("1"),
        "provider-driven DtmfMenu step trigger should contain digit '1'"
    );
    let next_entry = entries
        .iter()
        .find(|e| e.action_type == "Transfer")
        .expect("expected a Transfer trace entry");
    assert_eq!(
        next_entry.trigger.type_name(), "dtmf",
        "Transfer step (after provider-driven menu) should have trigger 'dtmf', got: {:?}",
        next_entry.trigger
    );
}

// ── A3: local-menu unmatched key surfaces a dtmf_menu_invalid trace ──

#[tokio::test]
async fn test_local_menu_unmatched_dtmf_invalid_trace() {
    use crate::call::app::ivr::trace::IvrTraceCollector;

    let trace = IvrTraceCollector::new();

    let mut entries = HashMap::new();
    entries.insert(
        "1".into(),
        ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        }),
    );

    let menu = ActionNode::new(EntryAction::DtmfMenu {
        greeting: Some("menu.wav".into()),
        greeting_text: None,
        greeting_record_list: None,
        greeting_voice: None,
        timeout_ms: 5000,
        max_retries: 3,
        entries,
        timeout_action: None,
        invalid_action: None,
        greeting_api_url: None,
    });

    let mut app: StepIvrApp = mock_app(vec![menu]);
    app.trace = Some(trace.clone());
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(c, CallCommand::Play { source: crate::call::domain::MediaSource::File { path }, .. } if path == "menu.wav")
        })
        .await;
    stack.audio_complete("ivr_menu_greeting");
    let _ = stack.drain_cmds();
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Unmatched key: menu must stay alive (no transfer) and the key must
    // still be reported as invalid input.
    stack.dtmf("9");
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let sessions = trace.sessions().await;
    let sess = &sessions[0];
    let entries = trace.query_by_session(&sess.session_id).await;
    let invalid = entries
        .iter()
        .find(|e| e.trigger.type_name() == "dtmf_menu_invalid")
        .expect("unmatched key must produce a dtmf_menu_invalid trace entry");
    assert_eq!(
        invalid
            .trigger.digit(),
        Some("9"),
        "dtmf_menu_invalid trace must carry the rejected digit"
    );
    assert_eq!(invalid.action_type, "DtmfMenu");

    // Contract: the rejected-key event carries the menu step's elapsed
    // time (start → key press), not a hardcoded 0.
    let invalid_duration = crate::call::app::ivr::trace::duration_ms_between(
        invalid.step_start_time.as_deref(),
        invalid
            .step_end_time
            .as_deref()
            .expect("invalid trace must carry step_end_time"),
    );
    assert_eq!(
        invalid.duration_ms, invalid_duration,
        "dtmf_menu_invalid duration_ms must equal step_end_time - step_start_time"
    );
    assert!(
        invalid.duration_ms >= 40,
        "dtmf_menu_invalid duration must reflect the menu's elapsed window (≥50ms sleep), \
         got {}ms",
        invalid.duration_ms
    );

    // The menu is still waiting: a subsequent valid key must resolve.
    stack.dtmf("1");
    stack
        .assert_cmd(2000, "stop", |c| {
            matches!(c, CallCommand::StopPlayback { .. })
        })
        .await;
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;
}

// ── A2: menu timeout overwrites the pending step trigger ──

#[tokio::test]
async fn test_provider_driven_menu_timeout_trace_trigger() {
    use crate::call::app::ivr::trace::IvrTraceCollector;

    let trace = IvrTraceCollector::new();
    let (provider, _events_handle) = capturing_menu_first();
    let mut app: StepIvrApp =
        StepIvrApp::with_provider(Box::new(provider)).with_name("timeout-trace-ivr");
    app.trace = Some(trace.clone());
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(c, CallCommand::Play { source: crate::call::domain::MediaSource::File { path }, .. } if path == "menu.wav")
        })
        .await;
    stack.audio_complete("ivr_menu_greeting");
    let _ = stack.drain_cmds();
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Fire the armed menu timeout → provider-driven path asks the
    // provider with DtmfMenuTimeout → Transfer.
    stack.timeout("ivr_dtmf_timeout");
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let sessions = trace.sessions().await;
    let sess = &sessions[0];
    let entries = trace.query_by_session(&sess.session_id).await;
    let menu_entry = entries
        .iter()
        .find(|e| e.action_type == "DtmfMenu")
        .expect("expected a DtmfMenu trace entry");
    assert_eq!(
        menu_entry.trigger.type_name(), "dtmf_menu_timeout",
        "menu step ended by timeout must carry trigger 'dtmf_menu_timeout' (symmetric with key presses), got: {:?}",
        menu_entry.trigger
    );
}

// ── B: InputPhone completion finalizes the wait with its original trigger ──

#[tokio::test]
async fn test_input_phone_completion_keeps_original_trigger() {
    use crate::call::app::ivr::trace::IvrTraceCollector;

    let trace = IvrTraceCollector::new();
    let app = mock_app(vec![
        ActionNode::new(EntryAction::InputPhone {
            prompt: None,
            prompt_text: None,
            prompt_voice: None,
            min_digits: 1,
            max_digits: 1,
            timeout_ms: 5000,
            inter_digit_timeout_ms: 3000,
            terminator: "#".into(),
        }),
        ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        }),
    ]);
    let mut app = app;
    app.trace = Some(trace.clone());
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    let _ = stack.drain_cmds();
    std::thread::sleep(std::time::Duration::from_millis(50));

    stack.dtmf("1");
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let sessions = trace.sessions().await;
    let sess = &sessions[0];
    let entries = trace.query_by_session(&sess.session_id).await;
    let finalized = entries
        .iter()
        .find(|e| e.trigger.type_name() == "phone_collected")
        .expect("InputPhone completion must emit a phone_collected entry");
    assert_eq!(
        finalized.action_type, "InputPhone",
        "the finalized entry must describe the step that finished"
    );
    assert!(
        finalized.step_end_time.is_some(),
        "the finalized entry must carry step_end_time (completion marker)"
    );
    assert!(
        !entries.iter().any(|e| e.trigger.type_name() == "session_end"
            && e.step_id.is_some()
            && e.end_reason.is_none()),
        "mid-flow wait completion must NOT be reported as session_end (reserved for on_exit with end_reason)"
    );
}

// ── Barge-in trace: DTMF during menu greeting ──

#[tokio::test]
async fn test_menu_tts_dtmf_during_greeting_trace() {
    use crate::call::app::ivr::trace::IvrTraceCollector;

    let trace = IvrTraceCollector::new();
    let (provider, events_handle) = capturing_menu_first();
    let mut app: StepIvrApp =
        StepIvrApp::with_provider(Box::new(provider)).with_name("bargein-trace-ivr");
    app.trace = Some(trace.clone());
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(c, CallCommand::Play { source: crate::call::domain::MediaSource::File { path }, .. } if path == "menu.wav")
        })
        .await;
    let _ = stack.drain_cmds();
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Barge-in while the greeting is still playing.
    stack.dtmf("2");
    stack
        .assert_cmd(2000, "stop", |c| {
            matches!(c, CallCommand::StopPlayback { .. })
        })
        .await;
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // No spurious AudioComplete may follow the DTMF (a stop must not be
    // reported as a natural playback completion).
    let events: Vec<String> = events_handle
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            Some(ProviderEvent::Dtmf { digit }) => Some(format!("dtmf:{digit}")),
            Some(ProviderEvent::AudioComplete { .. }) => Some("audio_complete".into()),
            _ => None,
        })
        .collect();
    let dtmf_pos = events
        .iter()
        .position(|e| e == "dtmf:2")
        .expect("DTMF must be forwarded");
    assert!(
        !events[dtmf_pos + 1..].iter().any(|e| e == "audio_complete"),
        "no AudioComplete may be forwarded to the provider after barge-in DTMF, got: {:?}",
        events
    );

    let sessions = trace.sessions().await;
    let sess = &sessions[0];
    let entries = trace.query_by_session(&sess.session_id).await;
    let menu_entry = entries
        .iter()
        .find(|e| e.action_type == "DtmfMenu")
        .expect("expected a DtmfMenu trace entry");
    assert_eq!(
        menu_entry.trigger.type_name(), "dtmf",
        "barge-in DtmfMenu step trigger should be 'dtmf', got: {:?}",
        menu_entry.trigger
    );
    assert_eq!(
        menu_entry
            .trigger.digit(),
        Some("2")
    );
}

// ── menu_tts_api: greeting fetched via API (greeting_api_url) ──

#[tokio::test]
async fn test_menu_tts_api_greeting_fetch_and_trace() {
    use crate::call::app::ivr::trace::IvrTraceCollector;
    use axum::{Router, routing::get};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let trace = IvrTraceCollector::new();
    let fetches = Arc::new(AtomicUsize::new(0));
    let fetches_for_handler = fetches.clone();
    let tts_app = Router::new().route(
        "/tts",
        get(move || {
            let fetches = fetches_for_handler.clone();
            async move {
                fetches.fetch_add(1, Ordering::SeqCst);
                axum::Json(serde_json::json!({"tts_text": "请按1"}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    crate::utils::spawn(async move {
        axum::serve(listener, tts_app).await.ok();
    });
    let greeting_api = format!("http://{}:{}/tts", addr.ip(), addr.port());

    let (provider, _events_handle) = capturing_menu_first();
    let mut app: StepIvrApp =
        StepIvrApp::with_provider(Box::new(provider)).with_name("menu-tts-api-ivr");
    app.trace = Some(trace.clone());
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(c, CallCommand::Play { source: crate::call::domain::MediaSource::File { path }, .. } if path == "menu.wav")
        })
        .await;
    stack.audio_complete("ivr_menu_greeting");
    let _ = stack.drain_cmds();
    std::thread::sleep(std::time::Duration::from_millis(50));
    stack.dtmf("1");

    stack
        .assert_cmd(2000, "stop", |c| {
            matches!(c, CallCommand::StopPlayback { .. })
        })
        .await;
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;
    // Sanity for the harness: the capturing_menu_first menu above does
    // not use greeting_api_url. This test's real assertion target is the
    // greeting-fetched menu below.
    drop(stack);

    // Now the actual greeting_api_url menu: the API text is fetched, and
    // with no TTS engine configured the menu degrades to a silent wait
    // (NoAudio path) but keys/timeouts/traces must behave identically.
    let trace2 = IvrTraceCollector::new();
    let menu_node = ActionNode::new(EntryAction::DtmfMenu {
        greeting: None,
        greeting_text: None,
        greeting_record_list: None,
        greeting_voice: None,
        timeout_ms: 30000,
        max_retries: 3,
        entries: HashMap::new(),
        timeout_action: None,
        invalid_action: None,
        greeting_api_url: Some(greeting_api),
    });
    let mut app2 = mock_app(vec![
        menu_node,
        ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        }),
    ]);
    app2.trace = Some(trace2.clone());
    let mut stack2 = MockCallStack::run(Box::new(app2), "1001", "2000");

    stack2
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    // The fetched text is synthesized via TTS when a TTS backend is
    // available (greeting plays), otherwise the menu degrades to a
    // silent wait (NoAudio path). Both paths must wait for keys.
    if let Some(CallCommand::Play { .. }) = stack2.next_cmd(5000).await {
        stack2.audio_complete("ivr_menu_greeting");
    }
    std::thread::sleep(std::time::Duration::from_millis(150));
    let _ = stack2.drain_cmds();
    stack2.dtmf("1");
    stack2
        .assert_cmd(2000, "stop", |c| {
            matches!(c, CallCommand::StopPlayback { .. })
        })
        .await;
    stack2
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    assert!(
        fetches.load(Ordering::SeqCst) >= 1,
        "greeting_api_url must be fetched when the menu executes"
    );

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let sessions = trace2.sessions().await;
    let sess = &sessions[0];
    let entries = trace2.query_by_session(&sess.session_id).await;
    let menu_entry = entries
        .iter()
        .find(|e| e.action_type == "DtmfMenu")
        .expect("expected a DtmfMenu trace entry");
    assert_eq!(
        menu_entry.trigger.type_name(), "dtmf",
        "menu_tts_api menu step trigger should be 'dtmf', got: {:?}",
        menu_entry.trigger
    );
    assert_eq!(
        menu_entry
            .trigger.digit(),
        Some("1")
    );
}

// ── Bridge return: buffered DTMF must reach the provider as `dtmf` ──
//
// Regression guard: `SipSession::handle_start_return_app` restarts this
// app with `ivr_params.bridge_dtmf_digits` after a voip_bridge ends, and
// those digits used to be merged into session variables WITHOUT ever
// being forwarded — the provider only ever saw session_start/session_end.

fn bridge_return_app(handle: &MockProviderHandle, digits: &str) -> StepIvrApp {
    StepIvrApp::with_provider(Box::new(MockProviderHandle(handle.0.clone())))
        .with_name("bridge-return-ivr")
        .with_ivr_params(serde_json::json!({
            "bridge_dtmf_digits": digits,
            "return_menu": "lf-step-ivr",
            // Lifecycle marker written by SipSession::resolve_return_app
            // from the executor's `return_ivr_resume=1` bridge param.
            "ivr_resumed": "1",
        }))
}

fn first_events(provider: &MockProvider, n: usize) -> Vec<String> {
    provider
        .events
        .lock()
        .unwrap()
        .iter()
        .take(n)
        .map(|e| match e {
            Some(ProviderEvent::Dtmf { digit }) => format!("dtmf:{digit}"),
            Some(ProviderEvent::SessionStart) => "session_start".into(),
            Some(ProviderEvent::Resume { .. }) => "resume".into(),
            Some(ProviderEvent::AudioComplete { .. }) => "audio_complete".into(),
            Some(other) => format!("{other:?}"),
            None => "none".into(),
        })
        .collect()
}

#[tokio::test]
async fn test_bridge_return_replays_first_digit_as_dtmf_trigger() {
    let provider = MockProvider::new(vec![ActionNode::new(EntryAction::Transfer {
        target: "2001".into(),
        headers: HashMap::new(),
        params: HashMap::new(),
        return_app: None,
        return_target: None,
    })]);
    let handle = MockProviderHandle(Arc::new(provider));

    let mut app = bridge_return_app(&handle, "1");
    let trace = crate::call::app::ivr::trace::IvrTraceCollector::new();
    app.trace = Some(trace.clone());
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(
            2000,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    let events = first_events(&handle.0, 2);
    assert_eq!(
        events.first().map(String::as_str),
        Some("dtmf:1"),
        "resumed app must deliver the buffered digit as the FIRST trigger, not session_start. events: {events:?}"
    );
    assert!(
        !events.contains(&"session_start".to_string()),
        "a bridge resume must not re-send session_start. events: {events:?}"
    );

    // Consumer contract (ivr_step_trace): the executed step carries
    // trigger.type == "dtmf" with detail.digit. (A resumed flow creates
    // no new collector session row — query by the fixed session id.)
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let entries = trace.query_by_session("test-session").await;
    let traced = entries
        .iter()
        .find(|e| e.trigger.type_name() == "dtmf")
        .expect("expected a trace entry triggered by dtmf");
    assert_eq!(
        traced
            .trigger.digit(),
        Some("1")
    );
}

/// Bridge return WITHOUT buffered digits (prompt finished / remote close /
/// timeout): the resumed instance's first provider event must be `resume`
/// — carrying `resume_from_step_id` from the proxy-attached bridge context
/// — never a second `session_start`, which is what made providers
/// re-enter the flow (the "caller hears the menu again" regression).
#[tokio::test]
async fn test_bridge_return_no_digits_sends_resume_event_with_step_id() {
    let provider = Arc::new(MockProvider::new(vec![ActionNode::new(EntryAction::Transfer {
        target: "2001".into(),
        headers: HashMap::new(),
        params: HashMap::new(),
        return_app: None,
        return_target: None,
    })]));

    let mut app = StepIvrApp::with_provider(Box::new(MockProviderHandle(provider.clone())))
        .with_name("bridge-return-ivr")
        .with_ivr_params(serde_json::json!({
            "ivr_resumed": "1",
            // Serialized bridge snapshot, as the proxy's return-app start
            // injects it on every bridge return (digit-carrying or not).
            "bridge_step_ctx": serde_json::json!({
                "step_id": "1000141102024500020003",
                "step_name": "菜单",
                "extra": { "nodetype": "menu_tts" },
                "step_start_time": "2026-01-01T00:00:00+00:00",
                "step_index": 2
            }).to_string()
        }));
    let trace = crate::call::app::ivr::trace::IvrTraceCollector::new();
    app.trace = Some(trace.clone());
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(
            2000,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    let events = first_events(&provider, 2);
    assert_eq!(
        events.first().map(String::as_str),
        Some("resume"),
        "a no-digit bridge resume must open with the resume event, got: {events:?}"
    );
    assert!(
        !events.contains(&"session_start".to_string()),
        "a resumed instance must never re-send session_start. events: {events:?}"
    );

    // The provider request must self-describe the resume even for a
    // stateless consumer: suspension point as a variable, status
    // `resuming` (not `starting`), and the step id on the wire event.
    let contexts = provider.contexts.lock().unwrap();
    let first_ctx = contexts.first().expect("provider must have been called");
    assert_eq!(
        first_ctx
            .variables
            .get("ivr_resume_from_step_id")
            .map(String::as_str),
        Some("1000141102024500020003"),
        "the bridge suspension point must be exposed as a variable"
    );
    assert_eq!(
        first_ctx.variables.get("ivr_status").map(String::as_str),
        Some("resuming"),
        "a resumed instance must not advertise itself as starting"
    );
    let wire_step_id = match &first_ctx.event {
        Some(ProviderEvent::Resume {
            resume_from_step_id,
        }) => resume_from_step_id.clone(),
        other => panic!("unexpected first provider event: {other:?}"),
    };
    assert_eq!(
        wire_step_id.as_deref(),
        Some("1000141102024500020003"),
        "the resume event must carry the bridge suspension point"
    );

    // Trace contract unchanged: the executed node's trigger is `resume`.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let entries = trace.query_by_session("test-session").await;
    let traced = entries
        .iter()
        .find(|e| e.trigger.type_name() == "resume")
        .expect("expected a trace entry triggered by resume");
    assert_eq!(
        traced
            .trigger.resume_step_id(),
        Some("1000141102024500020003")
    );
}

/// Resumable bridge return with stashed node context (`bridge_step_ctx`):
/// the proxy suppressed the eager per-digit trace for this hand-off, so
/// the resumed executor must report the bridge step itself — once the
/// successor node resolves — carrying the digit trigger AND the
/// successor's id as `next_node_id`/`next_step_id`.
#[tokio::test]
async fn test_bridge_return_reports_step_with_next_node_id() {
    let mut successor = ActionNode::new(EntryAction::Transfer {
        target: "2001".into(),
        headers: HashMap::new(),
        params: HashMap::new(),
        return_app: None,
        return_target: None,
    });
    successor.step_id = Some("1000141102024500020012".into());
    let provider = MockProvider::new(vec![successor]);
    let handle = MockProviderHandle(Arc::new(provider));

    let mut app = StepIvrApp::with_provider(Box::new(MockProviderHandle(handle.0.clone())))
        .with_name("bridge-resume-ctx-ivr")
        .with_ivr_params(serde_json::json!({
            "bridge_dtmf_digits": "8",
            "ivr_resumed": "1",
            // Serialized form, as the proxy's return-app start injects it
            // (`with_ivr_params` keeps string values only).
            "bridge_step_ctx": serde_json::json!({
                "step_id": "1000141102024500020003",
                "step_name": "菜单",
                "extra": { "nodetype": "menu_tts" },
                "step_start_time": "2026-01-01T00:00:00+00:00",
                "step_index": 2
            }).to_string()
        }));
    let trace = crate::call::app::ivr::trace::IvrTraceCollector::new();
    app.trace = Some(trace.clone());
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(
            2000,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let entries = trace.query_by_session("test-session").await;
    let bridge_entry = entries
        .iter()
        .find(|e| e.action_type == "Bridge")
        .expect("the suppressed bridge step must be reported by the resumed executor");
    assert_eq!(bridge_entry.step_id.as_deref(), Some("1000141102024500020003"));
    assert_eq!(bridge_entry.step_index, 2);
    assert_eq!(bridge_entry.trigger.type_name(), "dtmf");
    assert_eq!(
        bridge_entry
            .trigger.digit(),
        Some("8")
    );
    assert_eq!(
        bridge_entry.next_node_id.as_deref(),
        Some("1000141102024500020012"),
        "bridge step trace must carry the successor's step_id as next_node_id"
    );
    assert_eq!(
        bridge_entry.next_step_id.as_deref(),
        Some("1000141102024500020012")
    );
    assert_eq!(
        bridge_entry.step_start_time.as_deref(),
        Some("2026-01-01T00:00:00+00:00"),
        "start time must come from the stashed bridge context"
    );
    assert!(bridge_entry.step_end_time.is_some());
    assert!(
        bridge_entry.duration_ms > 0,
        "duration must derive from the stashed RFC3339 start (no live Instant on resume)"
    );

    // The successor's own entry keeps its own trigger + id, without a
    // next pointer (terminal transfer).
    let successor_entry = entries
        .iter()
        .find(|e| e.step_id.as_deref() == Some("1000141102024500020012"))
        .expect("the successor node must be traced");
    assert_eq!(successor_entry.trigger.type_name(), "dtmf");
    assert!(successor_entry.next_node_id.is_none());
}

#[tokio::test]
async fn test_bridge_return_extra_digits_flow_via_pending_buffer() {
    // "1,2": digit 1 is the resume trigger; digit 2 must be delivered on
    // the NEXT provider request (after the first node completes).
    let provider = MockProvider::new(vec![
        ActionNode::new(EntryAction::Prompt {
            file: Some("bridge_resume_prompt.wav".into()),
            tts_text: None,
            tts_voice: None,
            record_name_list: None,
            interruptible: false,
            tts_api_url: None,
            delay_before_ms: 0,
            delay_after_ms: 0,
        }),
        ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        }),
    ]);
    let handle = MockProviderHandle(Arc::new(provider));

    let app = bridge_return_app(&handle, "1,2");
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| matches!(c, CallCommand::Play { .. }))
        .await;
    stack.audio_complete("bridge_resume_prompt");
    stack
        .assert_cmd(
            2000,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    let events = first_events(&handle.0, 3);
    assert_eq!(
        events,
        vec!["dtmf:1".to_string(), "dtmf:2".to_string()],
        "buffered digits must replay in press order across steps. events: {events:?}"
    );
}

#[tokio::test]
async fn test_bridge_return_without_digits_uses_resume_trigger() {
    // No digits buffered while suspended: the provider protocol receives
    // the explicit `Resume` event (mirroring the lifecycle trace), and
    // the first node's trace trigger is `resume` — the flow already
    // emitted its session_start at the true first entry (exactly-once
    // contract). The legacy `session_start` wire shape is covered by the
    // StepProvider downgrade test (`resume_event_mode=session_start`).
    let provider = MockProvider::new(vec![ActionNode::new(EntryAction::Transfer {
        target: "2001".into(),
        headers: HashMap::new(),
        params: HashMap::new(),
        return_app: None,
        return_target: None,
    })]);
    let handle = MockProviderHandle(Arc::new(provider));

    let mut app = bridge_return_app(&handle, "");
    let trace = crate::call::app::ivr::trace::IvrTraceCollector::new();
    app.trace = Some(trace.clone());
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(
            2000,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    let events = first_events(&handle.0, 1);
    assert_eq!(
        events.first().map(String::as_str),
        Some("resume"),
        "a no-digit resume must open with the explicit Resume event. events: {events:?}"
    );

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let entries = trace.query_by_session("test-session").await;
    assert!(
        entries.iter().any(|e| e.trigger.type_name() == "resume"),
        "resumed first node must carry the `resume` lifecycle trigger, got: {:?}",
        entries
            .iter()
            .map(|e| e.trigger.type_name())
            .collect::<Vec<_>>()
    );
    assert!(
        !entries.iter().any(|e| e.trigger.type_name() == "session_start"),
        "a resumed flow must never re-emit session_start"
    );
}

// ── Timeout replay loop: each replay produces its own DtmfMenu trace ──

#[tokio::test]
async fn test_menu_tts_timeout_replay_produces_trace_per_cycle() {
    use crate::call::app::ivr::trace::IvrTraceCollector;

    let trace = IvrTraceCollector::new();
    // Initial step and first timeout replay the menu (like
    // ThirdPartyTreeProvider); then give up with a transfer.
    let replays = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (provider, _captured) =
        CapturingProvider::scripted(move |ctx, _| {
            if matches!(
                ctx.event,
                Some(ProviderEvent::SessionStart) | Some(ProviderEvent::DtmfMenuTimeout)
            ) && replays
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                <= 1
            {
                return Ok(test_menu_node(None));
            }
            Ok(test_transfer_node())
        });
    let mut app: StepIvrApp =
        StepIvrApp::with_provider(Box::new(provider)).with_name("replay-trace-ivr");
    app.trace = Some(trace.clone());
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(c, CallCommand::Play { source: crate::call::domain::MediaSource::File { path }, .. } if path == "menu.wav")
        })
        .await;

    // Cycle 1: greeting completes → timeout → provider replays menu.
    stack.audio_complete("ivr_menu_greeting");
    let _ = stack.drain_cmds();
    std::thread::sleep(std::time::Duration::from_millis(50));
    stack.timeout("ivr_dtmf_timeout");
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(c, CallCommand::Play { source: crate::call::domain::MediaSource::File { path }, .. } if path == "menu.wav")
        })
        .await;

    // Cycle 2: second timeout → provider gives up with Transfer.
    stack.timeout("ivr_dtmf_timeout");
    let _ = stack.drain_cmds();
    std::thread::sleep(std::time::Duration::from_millis(50));
    stack.timeout("ivr_dtmf_timeout");
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let sessions = trace.sessions().await;
    let sess = &sessions[0];
    let entries = trace.query_by_session(&sess.session_id).await;
    let menu_entries: Vec<_> = entries
        .iter()
        .filter(|e| e.action_type == "DtmfMenu")
        .collect();
    assert!(
        menu_entries.len() >= 2,
        "each menu cycle (initial + replay) must produce its own DtmfMenu trace, got: {:?}",
        entries
            .iter()
            .map(|e| (&e.action_type, e.trigger.type_name()))
            .collect::<Vec<_>>()
    );
    // The replayed menu step ended by the second timeout.
    assert!(
        menu_entries
            .iter()
            .any(|e| e.trigger.type_name() == "dtmf_menu_timeout"),
        "timeout-ended menu steps must carry trigger 'dtmf_menu_timeout'"
    );
}

#[tokio::test]
async fn test_hangup_no_prompt() {
    let mut stack = MockCallStack::run(
        Box::new(mock_app(vec![ActionNode::new(EntryAction::hangup_none())])),
        "1001",
        "2000",
    );
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "hangup", |c| matches!(c, CallCommand::Hangup(_)))
        .await;
}

#[tokio::test]
async fn test_jump_ivr() {
    let mut stack = MockCallStack::run(
        Box::new(mock_app(vec![ActionNode::new(EntryAction::JumpIvr {
            route_point: "39290".into(),
            params: HashMap::from([("businessType".into(), "7".into())]),
        })])),
        "1001",
        "2000",
    );
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "transfer", |c| {
            matches!(c, CallCommand::Transfer { target, .. }
                if target == "toivr:39290?businessType=7&_ivr_resume=1")
        })
        .await;
}

#[tokio::test]
async fn test_bridge() {
    let mut stack = MockCallStack::run(
        Box::new(mock_app(vec![ActionNode::new(EntryAction::Bridge {
            create_room_uri: "https://voip.example.com/rooms".into(),
            headers: HashMap::from([("Authorization".into(), "Bearer token".into())]),
            timeout_ms: Some(30000),
            return_app: None,
            return_target: None,
            success: None,
            failure: None,
        })])),
        "1001",
        "2000",
    );
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "transfer", |c| {
            matches!(c, CallCommand::Transfer { target, .. }
                if target.starts_with("bridge:https://voip.example.com/rooms?")
                    && {
                        let params: HashMap<&str, &str> = target
                            .split('?')
                            .nth(1)
                            .unwrap_or_default()
                            .split('&')
                            .filter_map(|p| p.split_once('='))
                            .collect();
                        let start_ok = chrono::DateTime::parse_from_rfc3339(
                            &urlencoding::decode(
                                params.get("_rst_step_start_time").copied().unwrap_or_default(),
                            )
                            .unwrap_or_default(),
                        )
                        .is_ok();
                        // First fetch → terminal records fetch+1 = 2; the
                        // stashed index must mirror that terminal trace.
                        let index_ok = params.get("_rst_step_index") == Some(&"2");
                        start_ok && index_ok
                    })
        })
        .await;
}

#[tokio::test]
async fn test_bridge_with_return_to_ivr() {
    // Bridge with return_to_ivr must append the resume marker as a query
    // string (common.rs execute_action: sep '?' when uri has no '?').
    let mut stack = MockCallStack::run(
        Box::new(mock_app(vec![ActionNode::new(EntryAction::Bridge {
            create_room_uri: "wss://voip.example.com/room1".into(),
            headers: HashMap::new(),
            timeout_ms: None,
            return_app: Some("ivr".into()),
            return_target: Some("main".into()),
            success: Some(Box::new(ActionNode::new(EntryAction::hangup_none()))),
            failure: None,
        })])),
        "1001",
        "2000",
    );
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "transfer", |c| {
            matches!(c, CallCommand::Transfer { target, .. }
                if target.starts_with("bridge:wss://voip.example.com/room1?return_app=ivr&return_target=main&return_ivr_resume=1&_rst_step_start_time="))
        })
        .await;
}

// ── Lifecycle exactly-once: resumable hand-offs suppress session_end ──

#[tokio::test]
async fn test_on_exit_suppresses_session_end_on_bridge_handoff() {
    use crate::call::app::ivr::trace::IvrTraceCollector;

    let mut app = mock_app(vec![ActionNode::new(EntryAction::Bridge {
        create_room_uri: "wss://voip.example.com/room1".into(),
        headers: HashMap::new(),
        timeout_ms: None,
        return_app: Some("ivr".into()),
        return_target: Some("main".into()),
        success: None,
        failure: None,
    })]);
    let trace = IvrTraceCollector::new();
    app.trace = Some(trace.clone());
    // Real identity wiring: on_enter picks the generation up from the
    // invocation, so the suspension stash and the `_rst_*` param both
    // carry it — the same way production wires a resumed generation.
    let mut ctx = make_test_context();
    ctx.invocation = Some(crate::call::app::AppInvocationContext {
        app_execution_id: 3,
        ..Default::default()
    });
    let shared_vars = ctx.session_vars.clone();
    let mut stack = MockCallStack::run_with_context(Box::new(app), ctx);
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "transfer", |c| {
            matches!(c, CallCommand::Transfer { target, .. }
                if target.starts_with("bridge:")
                    && target.contains("_rst_app_execution_id=3"))
        })
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let sessions = trace.sessions().await;
    let sess = &sessions[0];
    let entries = trace.query_by_session(&sess.session_id).await;
    assert!(
        !entries.iter().any(|e| e.trigger.type_name() == "session_end"),
        "bridge hand-off with return_app must NOT emit session_end (flow resumes), got: {:?}",
        entries
            .iter()
            .map(|e| e.trigger.type_name())
            .collect::<Vec<_>>()
    );
    // Suspended-flow identity stash: the compensating session_end the
    // proxy emits on a real hangup while suspended must key to THIS
    // execution (3), never the successor's.
    assert_eq!(
        shared_vars
            .get(IVR_SUSPENDED_EXECUTION_ID_KEY)
            .map(|v| v.value().clone()),
        Some("3".to_string()),
        "resumable hand-off must stash the suspended execution's identity"
    );
    // The node's own execution trace is still recorded — and because the
    // session_end is suppressed, the hand-off reason lives on it.
    let bridge = entries
        .iter()
        .find(|e| e.action_type == "Bridge")
        .expect("the Bridge node's execution trace must remain");
    assert_eq!(
        bridge.end_reason,
        Some(crate::call::app::ivr::provider::SessionEndTag::Transfer),
        "suspended bridge entry must carry the hand-off end reason"
    );
    assert!(
        bridge
            .end_detail
            .as_deref()
            .is_some_and(|d| d.starts_with("bridge:")),
        "bridge entry detail must carry the bridge target, got {:?}",
        bridge.end_detail
    );
    // The collector session row stays open until the flow truly ends.
    assert_eq!(
        sess.status, "active",
        "suspended flow must not close the collector session row"
    );
}

/// Ownership handover: as soon as ANY new instance enters (the successor
/// return app / jump target), the predecessor's suspended-identity stash
/// must be dropped — a later compensation must never attribute the
/// successor's end to the predecessor's execution.
#[tokio::test]
async fn test_on_enter_clears_suspended_execution_id_stash() {
    let ctx = make_test_context();
    let shared_vars = ctx.session_vars.clone();
    shared_vars.insert(
        IVR_SUSPENDED_EXECUTION_ID_KEY.to_string(),
        "3".to_string(),
    );

    // Empty flow: on_enter runs the ownership handover immediately.
    let mut stack = MockCallStack::run_with_context(Box::new(mock_app(vec![])), ctx);
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert!(
        shared_vars
            .get(IVR_SUSPENDED_EXECUTION_ID_KEY)
            .is_none(),
        "successor on_enter must clear the predecessor's identity stash"
    );
}

#[tokio::test]
async fn test_on_exit_suppresses_session_end_on_jumpivr() {
    use crate::call::app::ivr::trace::IvrTraceCollector;

    let mut app = mock_app(vec![ActionNode::new(EntryAction::JumpIvr {
        route_point: "39290".into(),
        params: HashMap::new(),
    })]);
    let trace = IvrTraceCollector::new();
    app.trace = Some(trace.clone());
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "transfer", |c| {
            matches!(c, CallCommand::Transfer { target, .. }
                if target.starts_with("toivr:39290"))
        })
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let sessions = trace.sessions().await;
    let sess = &sessions[0];
    let entries = trace.query_by_session(&sess.session_id).await;
    assert!(
        !entries.iter().any(|e| e.trigger.type_name() == "session_end"),
        "JumpIvr continues the SAME logical flow — no session_end at the jump, got: {:?}",
        entries
            .iter()
            .map(|e| e.trigger.type_name())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        sess.status, "active",
        "jump must not close the collector session row"
    );
    // The suppressed session_end's reason lives on the jump node's own
    // completion entry instead.
    let jump = entries
        .iter()
        .find(|e| e.action_type == "JumpIvr")
        .expect("JumpIvr node trace must exist");
    assert_eq!(
        jump.end_reason,
        Some(crate::call::app::ivr::provider::SessionEndTag::TransferToIvr),
        "jump entry must carry the hand-off end reason"
    );
    assert!(
        jump.end_detail
            .as_deref()
            .is_some_and(|d| d.starts_with("toivr:39290")),
        "jump entry detail must carry the toivr target, got {:?}",
        jump.end_detail
    );
}

#[tokio::test]
async fn ivr_exec_terminal_transfer_reports_transferred_and_session_end() {
    use crate::call::app::ivr::trace::IvrTraceCollector;

    // Regression guard for the suppression: a plain Transfer (no
    // return_app, not an IVR jump) is a true flow end — session_end stays.
    let mut app = mock_app(vec![ActionNode::new(EntryAction::Transfer {
        target: "2001".into(),
        headers: HashMap::new(),
        params: HashMap::new(),
        return_app: None,
        return_target: None,
    })]);
    let trace = IvrTraceCollector::new();
    app.trace = Some(trace.clone());
    let mut context = make_test_context();
    context.invocation = Some(crate::call::app::AppInvocationContext {
        app_execution_id: 3,
        ..Default::default()
    });
    let extensions = context.session_extensions.clone();
    extensions
        .write()
        .insert(crate::proxy::proxy_call::ivr_exec_hook::IvrExecState {
            app_execution_id: Some(3),
            request_id: "request-transfer".into(),
            held_leg: None,
            initiator_leg: crate::call::domain::LegId::from("callee"),
            webhook_url: None,
            app_name: "ivr".into(),
            metadata: serde_json::Value::Null,
        });
    let mut stack = MockCallStack::run_with_context(Box::new(app), context);
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(
            2000,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let sessions = trace.sessions().await;
    let sess = &sessions[0];
    let entries = trace.query_by_session(&sess.session_id).await;
    let session_end = entries
        .iter()
        .find(|e| e.trigger.type_name() == "session_end")
        .expect("terminal transfer must still record session_end");
    assert_eq!(sess.status, "transfer");
    assert_eq!(
        session_end.end_reason,
        Some(crate::call::app::ivr::provider::SessionEndTag::Transfer)
    );
    // A plain transfer is NOT a resumable hand-off: its node entry stays
    // reason-free — the session_end above is the only end-reason carrier.
    let node_entry = entries
        .iter()
        .find(|e| e.trigger.type_name() != "session_end")
        .expect("transfer node trace must exist");
    assert_eq!(node_entry.end_reason, None);
    assert_eq!(node_entry.end_detail, None);
    let guard = extensions.read();
    let result = guard
        .get::<crate::proxy::proxy_call::ivr_exec_hook::IvrExecResult>()
        .expect("terminal transfer must produce an ivr.exec result");
    assert_eq!(result.status, "transferred");
    assert_eq!(result.reason, "transfer");
}

#[tokio::test]
async fn test_session_start_recorded_once_across_resume() {
    use crate::call::app::ivr::trace::IvrTraceCollector;

    // The first app instance (fresh flow) creates the collector session
    // row; a resumed instance (`ivr_resumed=1`) must NOT add a second row
    // for the same session_id.
    let trace = IvrTraceCollector::new();

    let mut first = mock_app(vec![ActionNode::new(EntryAction::Bridge {
        create_room_uri: "wss://voip.example.com/room1".into(),
        headers: HashMap::new(),
        timeout_ms: None,
        return_app: Some("ivr".into()),
        return_target: Some("main".into()),
        success: None,
        failure: None,
    })]);
    first.trace = Some(trace.clone());
    let mut stack = MockCallStack::run(Box::new(first), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "transfer", |c| {
            matches!(c, CallCommand::Transfer { .. })
        })
        .await;

    // Resume in a second app instance sharing the same collector + session_id
    // (MockCallStack uses the same "test-session" call_info for both).
    let resumed =
        StepIvrApp::with_provider(Box::new(MockProvider::new(vec![ActionNode::new(
            EntryAction::Transfer {
                target: "2001".into(),
                headers: HashMap::new(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            },
        )])))
        .with_ivr_params(serde_json::json!({ "ivr_resumed": "1" }));
    let mut resumed = resumed;
    resumed.trace = Some(trace.clone());
    let mut stack2 = MockCallStack::run(Box::new(resumed), "1001", "2000");
    stack2
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack2
        .assert_cmd(2000, "transfer", |c| {
            matches!(c, CallCommand::Transfer { .. })
        })
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let sessions = trace.sessions().await;
    assert_eq!(
        sessions.len(),
        1,
        "a resumed flow must not create a second collector session row, got {} rows",
        sessions.len()
    );
}

#[tokio::test]
async fn test_route_to_agent() {
    // RouteToAgent is terminal: it substitutes the target and emits a plain
    // Transfer. The skill_group_id/key_id/channel_code are written to
    // internal session variables consumed by downstream routing (verified
    // end-to-end in the tier3 step-mode test).
    let mut stack = MockCallStack::run(
        Box::new(mock_app(vec![ActionNode::new(EntryAction::RouteToAgent {
            target: "1001".into(),
            skill_group_id: Some("support".into()),
            key_id: Some("night".into()),
            channel_code: Some("chat".into()),
        })])),
        "1001",
        "2000",
    );
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "1001"),
        )
        .await;
}

#[tokio::test]
async fn test_trace_integration() {
    use crate::call::app::ivr::trace::IvrTraceCollector;

    let trace = IvrTraceCollector::new();
    let mut app = mock_app(vec![ActionNode::new(EntryAction::Transfer {
        target: "2001".into(),
        headers: HashMap::new(),
        params: HashMap::new(),
        return_app: None,
        return_target: None,
    })]);
    app.trace = Some(trace.clone());
    app.ivr_name = Some("test-ivr".to_string());

    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    // Wait for async trace writes
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Verify trace was recorded
    let sessions = trace.sessions().await;
    assert!(!sessions.is_empty(), "expected at least one trace session");
    let sess = &sessions[0];
    assert_eq!(sess.caller, "1001");
    assert_eq!(sess.callee, "2000");
    assert_eq!(sess.ivr_name.as_deref(), Some("test-ivr"));
    assert_eq!(sess.status, "transfer");

    // Verify trace entries exist
    let entries = trace.query_by_session(&sess.session_id).await;
    assert!(!entries.is_empty(), "expected at least one trace entry");
    assert!(
        entries.iter().any(|e| e.action_type == "Transfer"),
        "expected a Transfer step entry"
    );
    // The session_end entry should carry the transfer end reason + detail.
    let session_end = entries
        .iter()
        .find(|e| e.trigger.type_name() == "session_end")
        .expect("expected a session_end trace entry");
    assert_eq!(
        session_end.end_reason,
        Some(crate::call::app::ivr::provider::SessionEndTag::Transfer)
    );
    assert_eq!(session_end.end_detail.as_deref(), Some("2001"));
    assert_eq!(
        session_end.action_type, "Transfer",
        "session_end entry should reuse the last node's action_type"
    );
}

#[tokio::test]
async fn test_next_chain_skip() {
    // When next is present, provider should NOT be called after completion
    let node = ActionNode::with_next(
        EntryAction::Prompt {
            file: Some("hello.wav".into()),
            tts_text: None,
            tts_voice: None,
            record_name_list: None,
            interruptible: false,
            tts_api_url: None,

            delay_before_ms: 0,
            delay_after_ms: 0,
        },
        ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        }),
    );

    let mut stack = MockCallStack::run(Box::new(mock_app(vec![node])), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path }, ..
                } if path == "hello.wav"
            )
        })
        .await;

    // After prompt completes, Transfer should fire WITHOUT provider call
    stack.audio_complete("ivr_prompt");
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;
}

#[tokio::test]
async fn test_error_action_returns_error() {
    // Mock provider that returns a VoipBridge (which succeeds) initially,
    // but we test with an InputVoice which returns an error
    let mut stack = MockCallStack::run(
        Box::new(mock_app(vec![ActionNode::new(EntryAction::InputVoice {
            scene: "test_scene".into(),
            timeout_ms: 5000,
        })])),
        "1001",
        "2000",
    );
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    // InputVoice should produce an answer error in the app
    // The app currently returns the error from execute_action
    // which gets propagated up through on_enter
}

// ── Integration tests with real HTTP StepProvider ─────────────────────

/// Start a lightweight HTTP server that returns pre-programmed responses.
async fn spawn_mock_provider(responses: Vec<serde_json::Value>) -> String {
    use axum::{Json, Router, routing::post};
    use std::sync::Mutex;

    let responses = Arc::new(Mutex::new(responses.into_iter()));
    let app = Router::new().route(
        "/ivr/step",
        post(move |Json(_body): Json<serde_json::Value>| {
            let resp = {
                let mut it = responses.lock().unwrap();
                it.next().unwrap_or(serde_json::json!({"type": "hangup"}))
            };
            async move { Json(resp) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    crate::utils::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    format!("http://{}:{}/ivr/step", addr.ip(), addr.port())
}

/// Mock provider with separate `/step` and `/fail` scripted responses.
async fn spawn_mock_provider_with_fail(
    step_responses: Vec<serde_json::Value>,
    fail_responses: Vec<Result<serde_json::Value, u16>>,
) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::post};
    use std::sync::Mutex;

    let paths = Arc::new(Mutex::new(Vec::<String>::new()));
    let paths_step = paths.clone();
    let paths_fail = paths.clone();
    let step_q = Arc::new(Mutex::new(step_responses.into_iter()));
    let fail_q = Arc::new(Mutex::new(fail_responses.into_iter()));

    let app = Router::new()
        .route(
            "/ivr/step",
            post(move |Json(_body): Json<serde_json::Value>| {
                paths_step.lock().unwrap().push("step".into());
                let resp = {
                    let mut it = step_q.lock().unwrap();
                    it.next().unwrap_or(serde_json::json!({"type": "hangup"}))
                };
                async move { Json(resp) }
            }),
        )
        .route(
            "/ivr/step/fail",
            post(move |Json(_body): Json<serde_json::Value>| {
                paths_fail.lock().unwrap().push("fail".into());
                let next = {
                    let mut it = fail_q.lock().unwrap();
                    it.next()
                };
                async move {
                    match next {
                        Some(Ok(body)) => Json(body).into_response(),
                        Some(Err(code)) => StatusCode::from_u16(code)
                            .unwrap_or(StatusCode::SERVICE_UNAVAILABLE)
                            .into_response(),
                        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
                    }
                }
            }),
        )
        .route(
            "/ivr/step/start",
            post(|| async { Json(serde_json::json!({"ok": true})) }),
        )
        .route(
            "/ivr/step/end",
            post(|| async { Json(serde_json::json!({"ok": true})) }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    crate::utils::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (
        format!("http://{}:{}/ivr/step", addr.ip(), addr.port()),
        paths,
    )
}

#[tokio::test]
async fn test_http_e2e_fail_recovers_with_transfer() {
    // /step returns tree-only `repeat` → execute fails → /fail returns transfer.
    let (url, paths) = spawn_mock_provider_with_fail(
        vec![serde_json::json!({"type": "repeat"})],
        vec![Ok(
            serde_json::json!({"type": "transfer", "target": "2001"}),
        )],
    )
    .await;

    let provider = StepProvider::new(&url, reqwest::Client::new()).with_retry(RetryConfig {
        max_retries: 1,
        timeout_ms: 2000,
        retry_delay_ms: 10,
        fallback_action: None,
    });
    let mut stack = MockCallStack::run(
        Box::new(StepIvrApp::with_provider(Box::new(provider)).with_name("fail-ok")),
        "1001",
        "2000",
    );
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(
            2000,
            "transfer after /fail",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    let seen = paths.lock().unwrap().clone();
    assert!(
        seen.iter().any(|p| p == "fail"),
        "expected /fail hit, paths={seen:?}"
    );
}

#[tokio::test]
async fn test_http_e2e_fail_down_jumps_ivr_fallback() {
    let (url, paths) = spawn_mock_provider_with_fail(
        vec![serde_json::json!({"type": "repeat"})],
        vec![Err(503)],
    )
    .await;

    let fb = Arc::new(crate::config::IvrFallbackConfig {
        default: Some("default_ivr".into()),
        rules: vec![crate::config::IvrFallbackRule {
            name: Some("vip".into()),
            priority: 10,
            match_conditions: crate::proxy::routing::MatchConditions {
                from_user: Some("1001".into()),
                ..Default::default()
            },
            target: "builtin_vip".into(),
        }],
    });
    let provider = StepProvider::new(&url, reqwest::Client::new())
        .with_retry(RetryConfig {
            max_retries: 1,
            timeout_ms: 500,
            retry_delay_ms: 10,
            fallback_action: None,
        })
        .with_prefer_ivr_fallback(true);
    let mut context = make_test_context();
    context.invocation = Some(crate::call::app::AppInvocationContext {
        app_execution_id: 2,
        callee: "39230".into(),
        sip_headers: HashMap::new(),
        variables: HashMap::from([
            ("caller".into(), "business-caller".into()),
            ("callee".into(), "business-callee".into()),
            ("session_id".into(), "business-session".into()),
        ]),
    });
    let mut stack = MockCallStack::run_with_context(
        Box::new(
            StepIvrApp::with_provider(Box::new(provider))
                .with_name("fail-fb")
                .with_ivr_fallback(Some(fb)),
        ),
        context,
    );
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "direct IVR fallback", |c| {
            matches!(
                c,
                CallCommand::Transfer { target, .. }
                    if target.starts_with("ivr:builtin_vip")
            )
        })
        .await;

    let seen = paths.lock().unwrap().clone();
    assert!(
        seen.iter().any(|p| p == "fail"),
        "expected /fail before fallback, paths={seen:?}"
    );
}

#[tokio::test]
async fn test_http_e2e_step_down_jumps_default_fallback() {
    use axum::{Router, http::StatusCode, response::IntoResponse, routing::post};

    let app = Router::new().route(
        "/ivr/step",
        post(|| async { StatusCode::SERVICE_UNAVAILABLE.into_response() }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    crate::utils::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    let url = format!("http://{}:{}/ivr/step", addr.ip(), addr.port());

    let fb = Arc::new(crate::config::IvrFallbackConfig {
        default: Some("default_ivr".into()),
        rules: vec![],
    });
    let provider = StepProvider::new(&url, reqwest::Client::new())
        .with_retry(RetryConfig {
            max_retries: 1,
            timeout_ms: 200,
            retry_delay_ms: 10,
            fallback_action: Some(ActionNode::new(EntryAction::Hangup {
                prompt: Some("sounds/error.wav".into()),
                prompt_text: None,
                prompt_voice: None,

                delay_before_ms: 0,
                delay_after_ms: 0,
            })),
        })
        .with_prefer_ivr_fallback(true);
    let mut stack = MockCallStack::run(
        Box::new(
            StepIvrApp::with_provider(Box::new(provider))
                .with_name("step-down")
                .with_ivr_fallback(Some(fb)),
        ),
        "1001",
        "2000",
    );
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    // prefer_ivr_fallback must skip retry.fallback hangup and use direct IVR instead.
    stack
        .assert_cmd(2000, "direct default IVR", |c| {
            matches!(
                c,
                CallCommand::Transfer { target, .. }
                    if target.starts_with("ivr:default_ivr")
            )
        })
        .await;
}

/// resume_event_mode = "session_start": the executor's `Resume` event is
/// rewritten to the legacy `session_start` on the wire WITHOUT any probe
/// or 400 round-trip — the escape hatch for providers that hard-fail on
/// unknown event types.
#[tokio::test]
async fn test_step_provider_session_start_mode_never_sends_resume() {
    use axum::{Router, response::IntoResponse, routing::post};

    let hits = Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let hits_for_server = hits.clone();
    let app = Router::new().route(
        "/ivr/step",
        post(move |body: String| {
            let hits = hits_for_server.clone();
            async move {
                let parsed: serde_json::Value =
                    serde_json::from_str(&body).unwrap_or_default();
                hits.lock().unwrap().push(parsed);
                axum::Json(serde_json::json!({
                    "type": "transfer",
                    "target": "2001"
                }))
                .into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    crate::utils::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    let url = format!("http://{}:{}/ivr/step", addr.ip(), addr.port());

    let provider = StepProvider::new(&url, reqwest::Client::new())
        .with_resume_event_mode(ResumeEventMode::SessionStart);
    let ctx = ProviderContext {
        session_id: "wire-mode-test".into(),
        app_execution_id: 2,
        caller: "1001".into(),
        callee: "2000".into(),
        direction: "internal".into(),
        tenant_id: None,
        ivr_id: None,
        variables: HashMap::new(),
        sip_headers: None,
        event: Some(ProviderEvent::Resume {
            resume_from_step_id: Some("1000141102024500020003".into()),
        }),
        route_name: None,
        custom_data: None,
        step_start_time: None,
        step_end_time: None,
        step_duration_ms: None,
        step_index: Some(0),
        transferred_from: None,
    };
    let node = provider.next_action(ctx).await.unwrap();
    assert!(
        matches!(node.action, EntryAction::Transfer { .. }),
        "session_start wire mode must still serve the flow"
    );

    let bodies = hits.lock().unwrap().clone();
    assert_eq!(bodies.len(), 1, "exactly one request, bodies={bodies:?}");
    assert_eq!(
        bodies[0]["event"]["type"], "session_start",
        "session_start wire mode must never emit resume on the wire"
    );
}

/// `ResumeEventMode::parse` contract: accepted spellings and rejection.
#[test]
fn test_resume_event_mode_parse() {
    assert_eq!(ResumeEventMode::parse("resume"), Some(ResumeEventMode::Resume));
    assert_eq!(
        ResumeEventMode::parse("session_start"),
        Some(ResumeEventMode::SessionStart)
    );
    assert_eq!(
        ResumeEventMode::parse("sessionstart"),
        Some(ResumeEventMode::SessionStart)
    );
    assert_eq!(ResumeEventMode::parse(" BOGUS "), None);
    assert_eq!(ResumeEventMode::parse(""), None);
}

/// Auto-downgrade fuse: a legacy `/step` endpoint that rejects the
/// `resume` event gets the request re-sent as `session_start` (single
/// probe attempt first — the caller must not pay the full retry budget
/// of dead air), and the endpoint is downgraded for the process lifetime
/// so later resumes skip the probe entirely.
#[tokio::test]
async fn test_step_provider_downgrades_rejected_resume_event() {
    use axum::{Router, http::StatusCode, response::IntoResponse, routing::post};

    crate::call::app::ivr::provider::reset_resume_downgrades_for_test();

    let hits = Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let hits_for_server = hits.clone();
    // Legacy facade: 400 on `type=resume`, transfer on anything else.
    let app = Router::new().route(
        "/ivr/step",
        post(move |body: String| {
            let hits = hits_for_server.clone();
            async move {
                let parsed: serde_json::Value =
                    serde_json::from_str(&body).unwrap_or_default();
                hits.lock().unwrap().push(parsed.clone());
                if parsed["event"]["type"] == "resume" {
                    StatusCode::BAD_REQUEST.into_response()
                } else {
                    axum::Json(serde_json::json!({
                        "type": "transfer",
                        "target": "2001"
                    }))
                    .into_response()
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    crate::utils::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    let url = format!("http://{}:{}/ivr/step", addr.ip(), addr.port());

    let provider = StepProvider::new(&url, reqwest::Client::new()).with_retry(RetryConfig {
        max_retries: 1,
        timeout_ms: 2000,
        retry_delay_ms: 10,
        fallback_action: None,
    });
    let mut stack = MockCallStack::run(
        Box::new(
            StepIvrApp::with_provider(Box::new(provider))
                .with_name("resume-downgrade")
                .with_ivr_params(serde_json::json!({ "ivr_resumed": "1" })),
        ),
        "1001",
        "2000",
    );
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(
            2000,
            "transfer after downgrade",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    let bodies = hits.lock().unwrap().clone();
    assert!(
        bodies.iter().any(|b| b["event"]["type"] == "resume"),
        "the resume event must be attempted first, bodies={bodies:?}"
    );
    assert!(
        bodies.iter().any(|b| b["event"]["type"] == "session_start"),
        "the rejected resume must be re-sent as session_start, bodies={bodies:?}"
    );

    // Fuse blown for this endpoint: a brand-new provider instance must
    // skip the resume probe entirely.
    let probe_provider = StepProvider::new(&url, reqwest::Client::new());
    let ctx = ProviderContext {
        session_id: "downgrade-probe".into(),
        app_execution_id: 2,
        caller: "1001".into(),
        callee: "2000".into(),
        direction: "internal".into(),
        tenant_id: None,
        ivr_id: None,
        variables: HashMap::new(),
        sip_headers: None,
        event: Some(ProviderEvent::Resume {
            resume_from_step_id: None,
        }),
        route_name: None,
        custom_data: None,
        step_start_time: None,
        step_end_time: None,
        step_duration_ms: None,
        step_index: Some(0),
        transferred_from: None,
    };
    let node = probe_provider.next_action(ctx).await.unwrap();
    assert!(
        matches!(node.action, EntryAction::Transfer { .. }),
        "downgraded endpoint must still serve the flow"
    );
    let bodies = hits.lock().unwrap().clone();
    let last = bodies.last().expect("probe must have hit the endpoint");
    assert_eq!(
        last["event"]["type"], "session_start",
        "downgraded endpoint must not be probed with resume again"
    );
    assert_eq!(
        bodies
            .iter()
            .filter(|b| b["event"]["type"] == "resume")
            .count(),
        1,
        "exactly one resume probe (from the full-flow episode) is expected"
    );
}

#[tokio::test]
async fn test_http_provider_full_flow() {
    // Provider returns: Prompt(with next:Transfer) → ...
    let entry = ActionNode::with_next(
        EntryAction::Prompt {
            file: Some("hello.wav".into()),
            tts_text: None,
            tts_voice: None,
            record_name_list: None,
            interruptible: false,
            tts_api_url: None,

            delay_before_ms: 0,
            delay_after_ms: 0,
        },
        ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        }),
    );
    let resp = serde_json::to_value(&entry).unwrap();

    let url = spawn_mock_provider(vec![resp]).await;
    let app = StepIvrApp::new(&url, reqwest::Client::new());

    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path }, ..
                } if path == "hello.wav"
            )
        })
        .await;
    // Audio complete triggers the next chain
    stack.audio_complete("ivr_prompt");
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;
}

#[tokio::test]
async fn test_http_provider_dtmf_flow() {
    // Provider returns DtmfMenu → user presses 1 → provider returns Transfer
    use std::collections::HashMap;

    let mut entries = HashMap::new();
    entries.insert(
        "1".into(),
        ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        }),
    );

    let menu_resp = ActionNode::new(EntryAction::DtmfMenu {
        greeting: Some("menu.wav".into()),
        greeting_text: None,
        greeting_record_list: None,
        greeting_voice: None,
        timeout_ms: 5000,
        max_retries: 3,
        entries,
        timeout_action: Some(Box::new(ActionNode::new(EntryAction::Repeat))),
        invalid_action: Some(Box::new(ActionNode::new(EntryAction::Repeat))),
        greeting_api_url: None,
    });

    let url = spawn_mock_provider(vec![serde_json::to_value(&menu_resp).unwrap()]).await;

    let app = StepIvrApp::new(&url, reqwest::Client::new());

    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path }, ..
                } if path == "menu.wav"
            )
        })
        .await;
    stack.audio_complete("ivr_menu_greeting");

    // DTMF "1" → local entries match → Transfer without provider call
    let _ = stack.drain_cmds();
    std::thread::sleep(std::time::Duration::from_millis(100));
    stack.dtmf("1");
    std::thread::sleep(std::time::Duration::from_millis(100));
    let _ = stack.drain_cmds();
    stack
        .assert_cmd(500, "stop", |c| {
            matches!(c, CallCommand::StopPlayback { .. })
        })
        .await;
    stack
        .assert_cmd(
            500,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;
}

#[tokio::test]
async fn test_http_provider_provider_called_after_menu() {
    // Provider returns DtmfMenu WITHOUT entries → user presses 1
    // → provider should be called with dtmf event
    let menu_resp = ActionNode::new(EntryAction::DtmfMenu {
        greeting: Some("menu.wav".into()),
        greeting_text: None,
        greeting_record_list: None,
        greeting_voice: None,
        timeout_ms: 5000,
        max_retries: 3,
        entries: std::collections::HashMap::new(),
        timeout_action: None,
        invalid_action: None,
        greeting_api_url: None,
    });
    let transfer_resp = ActionNode::new(EntryAction::Transfer {
        target: "2001".into(),
        headers: HashMap::new(),
        params: HashMap::new(),
        return_app: None,
        return_target: None,
    });

    let url = spawn_mock_provider(vec![
        serde_json::to_value(&menu_resp).unwrap(),
        serde_json::to_value(&transfer_resp).unwrap(),
    ])
    .await;

    let app = StepIvrApp::new(&url, reqwest::Client::new());

    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path }, ..
                } if path == "menu.wav"
            )
        })
        .await;
    stack.audio_complete("ivr_menu_greeting");

    // DTMF "1" → no local entry → provider should be called
    let _ = stack.drain_cmds();
    std::thread::sleep(std::time::Duration::from_millis(100));
    stack.dtmf("1");
    std::thread::sleep(std::time::Duration::from_millis(100));
    let _ = stack.drain_cmds();
    std::thread::sleep(std::time::Duration::from_millis(100));
    // After provider returns, StopPlayback + Transfer
    stack
        .assert_cmd(500, "stop", |c| {
            matches!(c, CallCommand::StopPlayback { .. })
        })
        .await;
    stack
        .assert_cmd(
            500,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;
}

// ── Early DTMF (awaiting_dtmf flag) tests ───────────────────────────────

/// DTMF pressed during a non-interruptible Prompt should be silently
/// ignored — the provider must NOT be called with the digit, and the flow
/// must continue normally after audio completes.
#[tokio::test]
async fn test_early_dtmf_during_prompt_is_ignored() {
    let prompt = ActionNode::new(EntryAction::Prompt {
        file: Some("hello.wav".into()),
        tts_text: None,
        tts_voice: None,
        record_name_list: None,
        interruptible: false,
        tts_api_url: None,

        delay_before_ms: 0,
        delay_after_ms: 0,
    });
    let transfer = ActionNode::new(EntryAction::Transfer {
        target: "2001".into(),
        headers: HashMap::new(),
        params: HashMap::new(),
        return_app: None,
        return_target: None,
    });

    let mut stack =
        MockCallStack::run(Box::new(mock_app(vec![prompt, transfer])), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path },
                    ..
                } if path == "hello.wav"
            )
        })
        .await;

    // User presses a key WHILE the prompt is still playing.
    // This must be ignored — no extra commands should be generated.
    let _ = stack.drain_cmds();
    stack.dtmf("5");
    // Give the event loop a moment to process the DTMF event.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // No stop/transfer/provider command should have been emitted.
    let cmds = stack.drain_cmds();
    assert!(
        cmds.is_empty(),
        "early DTMF should be ignored, but got commands: {cmds:?}"
    );

    // Now audio completes normally — flow should proceed to Transfer.
    stack.audio_complete("ivr_prompt");
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;
}

/// Same as above but the Prompt has `next` chained — verifying that the
/// chained node fires correctly after audio complete even if a stray DTMF
/// was received during playback.
#[tokio::test]
async fn test_early_dtmf_with_chained_next() {
    let node = ActionNode::with_next(
        EntryAction::Prompt {
            file: Some("intro.wav".into()),
            tts_text: None,
            tts_voice: None,
            record_name_list: None,
            interruptible: false,
            tts_api_url: None,

            delay_before_ms: 0,
            delay_after_ms: 0,
        },
        ActionNode::new(EntryAction::Transfer {
            target: "3003".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        }),
    );

    let mut stack = MockCallStack::run(Box::new(mock_app(vec![node])), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path },
                    ..
                } if path == "intro.wav"
            )
        })
        .await;

    // Stray DTMF during playback — should be ignored.
    let _ = stack.drain_cmds();
    stack.dtmf("9");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(stack.drain_cmds().is_empty());

    // Audio completes → chained Transfer fires.
    stack.audio_complete("ivr_prompt");
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "3003"),
        )
        .await;
}

/// Verify that DTMF still works correctly when the IVR IS expecting input
/// (i.e. after a DtmfMenu greeting finishes).  This is a regression guard
/// ensuring the `awaiting_dtmf` flag doesn't block legitimate input.
#[tokio::test]
async fn test_dtmf_accepted_after_menu_greeting() {
    let mut entries = HashMap::new();
    entries.insert(
        "1".into(),
        ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        }),
    );

    let menu = ActionNode::new(EntryAction::DtmfMenu {
        greeting: Some("menu.wav".into()),
        greeting_text: None,
        greeting_record_list: None,
        greeting_voice: None,
        timeout_ms: 5000,
        max_retries: 3,
        entries,
        timeout_action: Some(Box::new(ActionNode::new(EntryAction::Repeat))),
        invalid_action: Some(Box::new(ActionNode::new(EntryAction::Repeat))),
        greeting_api_url: None,
    });

    let mut stack = MockCallStack::run(Box::new(mock_app(vec![menu])), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path },
                    ..
                } if path == "menu.wav"
            )
        })
        .await;

    // Greeting finishes → awaiting_dtmf becomes true.
    stack.audio_complete("ivr_menu_greeting");

    // Now DTMF should be accepted (pending_menu is set, so it goes through
    // the local menu lookup path).
    let _ = stack.drain_cmds();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    stack.dtmf("1");

    stack
        .assert_cmd(2000, "stop", |c| {
            matches!(c, CallCommand::StopPlayback { .. })
        })
        .await;
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;
}

/// Regression (bug): after a valid DTMF resolves a DtmfMenu and moves the
/// flow to the next node, the old `ivr_dtmf_timeout` (armed when the
/// greeting finished) must be cancelled. If it is left running it fires
/// `dtmf_timeout` at the *current* node — e.g. a PROMPT_BREAK step before
/// the to-agent transfer — which the provider doesn't expect and answers
/// with hangup, killing the call before the transfer ever starts.
#[tokio::test]
async fn test_stale_menu_timeout_after_valid_dtmf_is_ignored() {
    let mut entries = HashMap::new();
    // Matched key → a non-terminal prompt chain (mirrors PROMPT_BREAK →
    // toagent_by_kfb). The stale menu timeout must NOT derail it.
    entries.insert(
        "1".into(),
        ActionNode::with_next(
            EntryAction::Prompt {
                file: Some("prompt_break.wav".into()),
                tts_text: None,
                tts_voice: None,
                record_name_list: None,
                interruptible: false,
                tts_api_url: None,

                delay_before_ms: 0,
                delay_after_ms: 0,
            },
            ActionNode::new(EntryAction::Transfer {
                target: "2001".into(),
                headers: HashMap::new(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }),
        ),
    );

    let menu = ActionNode::new(EntryAction::DtmfMenu {
        greeting: Some("menu.wav".into()),
        greeting_text: None,
        greeting_record_list: None,
        greeting_voice: None,
        timeout_ms: 3000,
        max_retries: 3,
        entries,
        timeout_action: None,
        invalid_action: None,
        greeting_api_url: None,
    });

    let mut stack = MockCallStack::run(Box::new(mock_app(vec![menu])), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path }, ..
                } if path == "menu.wav"
            )
        })
        .await;

    // Greeting finishes → menu waits for input and arms ivr_dtmf_timeout.
    stack.audio_complete("ivr_menu_greeting");
    let _ = stack.drain_cmds();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Valid key press → local menu match → next node (prompt_break) runs.
    stack.dtmf("1");
    stack
        .assert_cmd(2000, "stop", |c| {
            matches!(c, CallCommand::StopPlayback { .. })
        })
        .await;
    stack
        .assert_cmd(2000, "play prompt_break", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path }, ..
                } if path == "prompt_break.wav"
            )
        })
        .await;

    // The stale 3s menu timeout now fires while prompt_break is playing.
    // It must be ignored — no hangup, no provider contact, no commands.
    stack.timeout("ivr_dtmf_timeout");
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let cmds = stack.drain_cmds();
    assert!(
        cmds.is_empty(),
        "stale menu timeout must not derail the current node, got: {cmds:?}"
    );

    // The current node chain completes normally → to-agent transfer fires.
    stack.audio_complete("ivr_prompt");
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;
}

/// A *legitimate* menu timeout (no DTMF pressed, pending_menu still set)
/// must keep working: the timeout_action executes and the flow moves on.
/// Guards against the stale-timeout guard accidentally swallowing real
/// timeouts.
#[tokio::test]
async fn test_local_menu_legit_timeout_fires_timeout_action() {
    let mut entries = HashMap::new();
    entries.insert(
        "1".into(),
        ActionNode::new(EntryAction::Transfer {
            target: "2002".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        }),
    );

    let menu = ActionNode::new(EntryAction::DtmfMenu {
        greeting: Some("menu.wav".into()),
        greeting_text: None,
        greeting_record_list: None,
        greeting_voice: None,
        timeout_ms: 3000,
        max_retries: 3,
        entries,
        timeout_action: Some(Box::new(ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        }))),
        invalid_action: None,
        greeting_api_url: None,
    });

    let mut stack = MockCallStack::run(Box::new(mock_app(vec![menu])), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path }, ..
                } if path == "menu.wav"
            )
        })
        .await;
    stack.audio_complete("ivr_menu_greeting");
    let _ = stack.drain_cmds();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // No DTMF — the menu genuinely times out → timeout_action (Transfer) runs.
    stack.timeout("ivr_dtmf_timeout");
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;
}

/// Provider-driven DtmfMenu: a DTMF digit is forwarded to the provider and
/// the flow moves to a non-terminal node. The stale `ivr_dtmf_timeout`
/// armed while the menu waited must be cancelled so it can't fire
/// `dtmf_timeout` at the provider-driven node.
#[tokio::test]
async fn test_provider_driven_menu_dtmf_no_stale_timeout() {
    // Provider sequence: session_start → provider-driven DtmfMenu (empty
    // entries); dtmf → non-terminal Prompt chained to Transfer.
    let menu = ActionNode::new(EntryAction::DtmfMenu {
        greeting: Some("menu.wav".into()),
        greeting_text: None,
        greeting_record_list: None,
        greeting_voice: None,
        timeout_ms: 3000,
        max_retries: 3,
        entries: HashMap::new(),
        timeout_action: None,
        invalid_action: None,
        greeting_api_url: None,
    });
    let prompt = ActionNode::with_next(
        EntryAction::Prompt {
            file: Some("prompt_break.wav".into()),
            tts_text: None,
            tts_voice: None,
            record_name_list: None,
            interruptible: false,
            tts_api_url: None,

            delay_before_ms: 0,
            delay_after_ms: 0,
        },
        ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        }),
    );

    let mut stack = MockCallStack::run(Box::new(mock_app(vec![menu, prompt])), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path }, ..
                } if path == "menu.wav"
            )
        })
        .await;
    stack.audio_complete("ivr_menu_greeting");
    let _ = stack.drain_cmds();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Digit forwarded to provider → provider returns prompt_break node.
    stack.dtmf("1");
    stack
        .assert_cmd(2000, "stop", |c| {
            matches!(c, CallCommand::StopPlayback { .. })
        })
        .await;
    stack
        .assert_cmd(2000, "play prompt_break", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path }, ..
                } if path == "prompt_break.wav"
            )
        })
        .await;

    // Stale menu timeout must be ignored — no commands emitted.
    stack.timeout("ivr_dtmf_timeout");
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let cmds = stack.drain_cmds();
    assert!(
        cmds.is_empty(),
        "stale menu timeout must not derail provider-driven node, got: {cmds:?}"
    );

    // prompt_break chain completes → Transfer fires.
    stack.audio_complete("ivr_prompt");
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;
}

/// Local menu with no timeout_action: each timeout must retry silently
/// (re-arm the timeout and stay in the menu) instead of forwarding
/// DtmfTimeout to the provider, which would leave stale pending_menu
/// intercepting future DTMF.
#[tokio::test]
async fn test_local_menu_timeout_retry_without_timeout_action() {
    let mut entries = HashMap::new();
    entries.insert(
        "1".into(),
        ActionNode::new(EntryAction::Transfer {
            target: "2002".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        }),
    );

    let menu = ActionNode::new(EntryAction::DtmfMenu {
        greeting: Some("menu.wav".into()),
        greeting_text: None,
        greeting_record_list: None,
        greeting_voice: None,
        timeout_ms: 100,
        max_retries: 2,
        entries,
        timeout_action: None,
        invalid_action: None,
        greeting_api_url: None,
    });

    let mut stack = MockCallStack::run(Box::new(mock_app(vec![menu])), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path }, ..
                } if path == "menu.wav"
            )
        })
        .await;
    stack.audio_complete("ivr_menu_greeting");
    let _ = stack.drain_cmds();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // First timeout (retry_count 0→1, not yet >= max_retries=2):
    // must stay in menu, re-arm timeout, no commands emitted.
    stack.timeout("ivr_dtmf_timeout");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let cmds = stack.drain_cmds();
    assert!(
        cmds.is_empty(),
        "timeout retry without timeout_action must not contact provider, got: {cmds:?}"
    );

    // Second timeout (retry_count 1→2, >= max_retries=2):
    // must fall through to Hangup.
    stack.timeout("ivr_dtmf_timeout");
    stack
        .assert_cmd(2000, "hangup", |c| matches!(c, CallCommand::Hangup(_)))
        .await;
}

/// Regression (bug): pressing a NON-matching key while a DtmfMenu greeting
/// is still playing must NOT stop the greeting and must NOT be forwarded to
/// the provider. Previously the unmatched digit fell through to the
/// provider, which could return a terminal node and silently end the IVR.
/// Here the provider's fallback node is a `Hangup` to simulate that
/// scenario — with the fix the flow stays alive and the greeting finishes.
#[tokio::test]
async fn test_non_matching_dtmf_during_menu_greeting_keeps_flow_alive() {
    let mut entries = HashMap::new();
    entries.insert(
        "1".into(),
        ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        }),
    );

    let menu = ActionNode::new(EntryAction::DtmfMenu {
        greeting: Some("menu.wav".into()),
        greeting_text: None,
        greeting_record_list: None,
        greeting_voice: None,
        timeout_ms: 5000,
        max_retries: 3,
        entries,
        timeout_action: None,
        invalid_action: None,
        greeting_api_url: None,
    });
    // Fallback provider node — would be executed only if the bug forwarded
    // the stray digit to the provider.
    let hangup = ActionNode::new(EntryAction::hangup_none());

    let mut stack = MockCallStack::run(Box::new(mock_app(vec![menu, hangup])), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path },
                    ..
                } if path == "menu.wav"
            )
        })
        .await;

    // Press a non-matching key WHILE the greeting is still playing.
    let _ = stack.drain_cmds();
    stack.dtmf("5");
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // Nothing should have happened: no StopPlayback, no provider call, no
    // Hangup. The greeting must continue uninterrupted.
    let cmds = stack.drain_cmds();
    assert!(
        cmds.is_empty(),
        "non-matching DTMF during greeting must not stop playback or contact provider, got: {cmds:?}"
    );

    // Greeting finishes normally → menu enters the waiting state.
    stack.audio_complete("ivr_menu_greeting");
    let _ = stack.drain_cmds();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // A subsequent VALID key must still work — proving the flow survived.
    stack.dtmf("1");
    stack
        .assert_cmd(2000, "stop", |c| {
            matches!(c, CallCommand::StopPlayback { .. })
        })
        .await;
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;
}

/// Regression (bug): a non-matching key pressed while the menu is in its
/// waiting window (greeting finished) and no `invalid_action` is configured
/// must be ignored, keeping the flow alive for a subsequent valid key.
#[tokio::test]
async fn test_non_matching_dtmf_in_menu_waiting_keeps_flow_alive() {
    let mut entries = HashMap::new();
    entries.insert(
        "1".into(),
        ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        }),
    );

    let menu = ActionNode::new(EntryAction::DtmfMenu {
        greeting: Some("menu.wav".into()),
        greeting_text: None,
        greeting_record_list: None,
        greeting_voice: None,
        timeout_ms: 5000,
        max_retries: 3,
        entries,
        timeout_action: None,
        invalid_action: None,
        greeting_api_url: None,
    });
    let hangup = ActionNode::new(EntryAction::hangup_none());

    let mut stack = MockCallStack::run(Box::new(mock_app(vec![menu, hangup])), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path },
                    ..
                } if path == "menu.wav"
            )
        })
        .await;
    // Greeting finished → waiting for input.
    stack.audio_complete("ivr_menu_greeting");
    let _ = stack.drain_cmds();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Non-matching key during the waiting window — must NOT be forwarded.
    stack.dtmf("5");
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let cmds = stack.drain_cmds();
    assert!(
        cmds.is_empty(),
        "non-matching DTMF in waiting window must not contact provider, got: {cmds:?}"
    );

    // Valid key still works → flow is alive.
    stack.dtmf("1");
    stack
        .assert_cmd(2000, "stop", |c| {
            matches!(c, CallCommand::StopPlayback { .. })
        })
        .await;
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;
}

/// Regression: DTMF during an *interruptible* Prompt must be forwarded to
/// the provider.  The `awaiting_dtmf` / `interrupt_on_dtmf` flags should
/// NOT block legitimate input when the node explicitly allows interruption.
#[tokio::test]
async fn test_interruptible_prompt_forwards_dtmf_to_provider() {
    let prompt = ActionNode::new(EntryAction::Prompt {
        file: Some("hello.wav".into()),
        tts_text: None,
        tts_voice: None,
        record_name_list: None,
        interruptible: true,
        tts_api_url: None,

        delay_before_ms: 0,
        delay_after_ms: 0,
    });
    let transfer = ActionNode::new(EntryAction::Transfer {
        target: "2001".into(),
        headers: HashMap::new(),
        params: HashMap::new(),
        return_app: None,
        return_target: None,
    });

    let mut stack =
        MockCallStack::run(Box::new(mock_app(vec![prompt, transfer])), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path },
                    ..
                } if path == "hello.wav"
            )
        })
        .await;

    // While the interruptible prompt is still playing, inject a DTMF digit.
    // It MUST be forwarded to the provider (not silently ignored).
    let _ = stack.drain_cmds();
    stack.dtmf("5");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The app should stop the audio first, then ask the provider for
    // the next action (MockProvider returns Transfer("2001")).
    stack
        .assert_cmd(2000, "stop", |c| {
            matches!(c, CallCommand::StopPlayback { .. })
        })
        .await;
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;
}

// ── interruptible parameter verification tests ─────────────────────────

/// The Play command emitted for a **non-interruptible** Prompt must carry
/// `interrupt_on_dtmf: false` in its PlayOptions.
#[tokio::test]
async fn test_non_interruptible_prompt_play_cmd_has_flag_false() {
    let node = ActionNode::new(EntryAction::Prompt {
        file: Some("hello.wav".into()),
        tts_text: None,
        tts_voice: None,
        record_name_list: None,
        interruptible: false,
        tts_api_url: None,

        delay_before_ms: 0,
        delay_after_ms: 0,
    });

    let mut stack = MockCallStack::run(Box::new(mock_app(vec![node])), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;

    // Grab the Play command and inspect its options.
    let play_cmd = stack.next_cmd(2000).await.expect("expected a Play command");
    match &play_cmd {
        CallCommand::Play { options, .. } => {
            let opts = options.as_ref().expect("PlayOptions must be set");
            assert!(
                !opts.interrupt_on_dtmf,
                "non-interruptible Prompt must have interrupt_on_dtmf=false, got true"
            );
        }
        other => panic!("expected Play command, got {other:?}"),
    }
}

/// The Play command emitted for an **interruptible** Prompt must carry
/// `interrupt_on_dtmf: true` in its PlayOptions.
#[tokio::test]
async fn test_interruptible_prompt_play_cmd_has_flag_true() {
    let node = ActionNode::new(EntryAction::Prompt {
        file: Some("hello.wav".into()),
        tts_text: None,
        tts_voice: None,
        record_name_list: None,
        interruptible: true,
        tts_api_url: None,

        delay_before_ms: 0,
        delay_after_ms: 0,
    });

    let mut stack = MockCallStack::run(Box::new(mock_app(vec![node])), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;

    let play_cmd = stack.next_cmd(2000).await.expect("expected a Play command");
    match &play_cmd {
        CallCommand::Play { options, .. } => {
            let opts = options.as_ref().expect("PlayOptions must be set");
            assert!(
                opts.interrupt_on_dtmf,
                "interruptible Prompt must have interrupt_on_dtmf=true, got false"
            );
        }
        other => panic!("expected Play command, got {other:?}"),
    }
}

/// After an interruptible Prompt finishes naturally (no DTMF), a chained
/// non-interruptible Prompt must NOT be interruptible — the
/// `interrupt_on_dtmf` flag must not leak from the previous node.
#[tokio::test]
async fn test_interruptible_then_non_interruptible_no_leak() {
    let node = ActionNode::with_next(
        EntryAction::Prompt {
            file: Some("first.wav".into()),
            tts_text: None,
            tts_voice: None,
            record_name_list: None,
            interruptible: true,
            tts_api_url: None,

            delay_before_ms: 0,
            delay_after_ms: 0,
        },
        ActionNode::new(EntryAction::Prompt {
            file: Some("second.wav".into()),
            tts_text: None,
            tts_voice: None,
            record_name_list: None,
            interruptible: false,
            tts_api_url: None,

            delay_before_ms: 0,
            delay_after_ms: 0,
        }),
    );

    let mut stack = MockCallStack::run(Box::new(mock_app(vec![node])), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    // First prompt (interruptible) starts playing.
    stack
        .assert_cmd(2000, "play first", |c| {
            matches!(c, CallCommand::Play { source, options, .. }
                if matches!(source, crate::call::domain::MediaSource::File { path } if path == "first.wav")
                && options.as_ref().map_or(false, |o| o.interrupt_on_dtmf))
        })
        .await;

    // First prompt finishes naturally — no DTMF pressed.
    stack.audio_complete("ivr_prompt");

    // Second prompt (non-interruptible) starts playing.
    stack
        .assert_cmd(2000, "play second", |c| {
            matches!(c, CallCommand::Play { source, options, .. }
                if matches!(source, crate::call::domain::MediaSource::File { path } if path == "second.wav")
                && options.as_ref().map_or(false, |o| !o.interrupt_on_dtmf))
        })
        .await;

    // Press a key during the second (non-interruptible) prompt.
    let _ = stack.drain_cmds();
    stack.dtmf("5");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // No stop / transfer / any command should have been emitted.
    let cmds = stack.drain_cmds();
    assert!(
        cmds.is_empty(),
        "DTMF during non-interruptible prompt (after interruptible) should be ignored, got: {cmds:?}"
    );

    // Audio completes normally → provider is asked for next node.
    // MockProvider has no more nodes → fallback plays error.wav then hangup.
    stack.audio_complete("ivr_prompt");
    stack
        .assert_cmd(500, "fallback play", |c| {
            matches!(c, CallCommand::Play { source, .. }
                if matches!(source, crate::call::domain::MediaSource::File { path } if path.contains("error.wav")))
        })
        .await;
    stack.audio_complete("ivr_prompt");
    stack
        .assert_cmd(500, "fallback hangup", |c| {
            matches!(c, CallCommand::Hangup(_))
        })
        .await;
}

/// Multiple DTMF digits pressed during a non-interruptible Prompt must
/// ALL be silently ignored — none should trigger stop_audio or a provider
/// call.
#[tokio::test]
async fn test_multiple_dtmf_during_non_interruptible_all_ignored() {
    let node = ActionNode::with_next(
        EntryAction::Prompt {
            file: Some("hello.wav".into()),
            tts_text: None,
            tts_voice: None,
            record_name_list: None,
            interruptible: false,
            tts_api_url: None,

            delay_before_ms: 0,
            delay_after_ms: 0,
        },
        ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        }),
    );

    let mut stack = MockCallStack::run(Box::new(mock_app(vec![node])), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| matches!(c, CallCommand::Play { .. }))
        .await;

    // Hammer multiple digits while playback is in progress.
    let _ = stack.drain_cmds();
    for d in &["1", "2", "3", "*", "#"] {
        stack.dtmf(*d);
    }
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let cmds = stack.drain_cmds();
    assert!(
        cmds.is_empty(),
        "all DTMF during non-interruptible prompt should be ignored, got: {cmds:?}"
    );

    // After natural completion, the chained Transfer fires.
    stack.audio_complete("ivr_prompt");
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;
}

/// JSON without an explicit `interruptible` field must default to `false`
/// (non-interruptible) per the `#[serde(default)]` attribute.
#[test]
fn test_prompt_json_without_interruptible_defaults_false() {
    let json = r#"{"type":"prompt","file":"hello.wav"}"#;
    let node: ActionNode = serde_json::from_str(json).expect("parse JSON");
    match node.action {
        EntryAction::Prompt { interruptible, .. } => {
            assert!(
                !interruptible,
                "missing `interruptible` must default to false"
            );
        }
        other => panic!("expected Prompt action, got {other:?}"),
    }
}

/// JSON with `"interruptible": true` must parse correctly.
#[test]
fn test_prompt_json_with_interruptible_true() {
    let json = r#"{"type":"prompt","file":"hello.wav","interruptible":true}"#;
    let node: ActionNode = serde_json::from_str(json).expect("parse JSON");
    match node.action {
        EntryAction::Prompt { interruptible, .. } => {
            assert!(interruptible, "interruptible=true must parse");
        }
        other => panic!("expected Prompt action, got {other:?}"),
    }
}

#[tokio::test]
async fn ivr_exec_remote_hangup_reports_failed_and_notifies_provider() {
    let provider = Arc::new(MockProvider::new(vec![ActionNode::new(
        EntryAction::Prompt {
            file: Some("hello.wav".into()),
            tts_text: None,
            tts_voice: None,
            record_name_list: None,
            interruptible: false,
            tts_api_url: None,

            delay_before_ms: 0,
            delay_after_ms: 0,
        },
    )]));
    let mut app = StepIvrApp::with_provider(Box::new(MockProviderHandle(provider.clone())));
    app.ivr_name = Some("test-ivr".to_string());
    let extensions = crate::proxy::proxy_call::session_hooks::SessionExtensions::new();
    extensions
        .write()
        .insert(crate::proxy::proxy_call::ivr_exec_hook::IvrExecState {
            app_execution_id: Some(4),
            request_id: "request-hangup".into(),
            held_leg: None,
            initiator_leg: crate::call::domain::LegId::from("callee"),
            webhook_url: None,
            app_name: "ivr".into(),
            metadata: serde_json::Value::Null,
        });
    app.session_extensions = Some(extensions.clone());
    app.app_execution_id = Some(4);
    app.sess
        .variables
        .insert("session_id".into(), "test-session".into());

    app.provider_session = Some(SessionContext {
        app_execution_id: 4,
        ..app.build_session_context_from_vars()
    });

    app.on_exit(crate::call::app::ExitReason::RemoteHangup(None))
        .await
        .expect("remote hangup exit should succeed");

    assert!(
        *provider.end_called.lock().unwrap(),
        "caller hangup is a session end — the provider must receive /end (user_hangup)"
    );
    let guard = extensions.read();
    let result = guard
        .get::<crate::proxy::proxy_call::ivr_exec_hook::IvrExecResult>()
        .expect("caller hangup must produce an ivr.exec result");
    assert_eq!(result.status, "failed");
    assert_eq!(result.reason, "remote_hangup");
}

#[tokio::test]
async fn test_session_end_trace_recorded_on_remote_hangup() {
    use crate::call::app::ivr::trace::IvrTraceCollector;

    let trace = IvrTraceCollector::new();
    let provider = Arc::new(MockProvider::new(vec![ActionNode::new(
        EntryAction::Transfer {
            target: "2001".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        },
    )]));
    let mut app = StepIvrApp::with_provider(Box::new(MockProviderHandle(provider.clone())));
    app.trace = Some(trace.clone());
    app.ivr_name = Some("test-ivr".to_string());
    let mut current_node = ActionNode::new(EntryAction::Transfer {
        target: "2001".into(),
        headers: HashMap::new(),
        params: HashMap::new(),
        return_app: None,
        return_target: None,
    });
    current_node.extra = Some(serde_json::json!({"nodetype": "transfer"}));
    app.current_node = Some(current_node);
    app.extra = Some(serde_json::json!({"nodetype": "previous-node"}));
    app.current_step_id = Some("step-7".to_string());
    app.current_step_start_time = Some("2026-01-01T00:00:00+00:00".to_string());
    app.sess
        .variables
        .insert("session_id".into(), "test-session".into());

    app.provider_session = Some(SessionContext {
        app_execution_id: 1,
        ..app.build_session_context_from_vars()
    });

    app.on_exit(crate::call::app::ExitReason::RemoteHangup(None))
        .await
        .expect("remote hangup exit should succeed");

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let entries = trace.query_by_session("test-session").await;
    let session_end = entries
        .iter()
        .find(|e| e.trigger.type_name() == "session_end")
        .expect("remote hangup must record a session_end trace entry");
    assert_eq!(
        session_end.step_start_time.as_deref(),
        Some("2026-01-01T00:00:00+00:00"),
        "session_end trace must carry the in-flight step's start time (duration_ms contract)"
    );
    // Contract: duration_ms is derived from the stamps, not hardcoded 0.
    // The start is a fixed past date, so the derived window is large.
    let expected = crate::call::app::ivr::trace::duration_ms_between(
        session_end.step_start_time.as_deref(),
        session_end
            .step_end_time
            .as_deref()
            .expect("session_end must carry step_end_time"),
    );
    assert_eq!(
        session_end.duration_ms, expected,
        "session_end duration_ms must equal step_end_time - step_start_time"
    );
    assert!(
        session_end.duration_ms > 0,
        "session_end duration must be derived from its stamps, got 0"
    );
    assert_eq!(
        session_end.step_id.as_deref(),
        Some("step-7"),
        "session_end trace must record the last executed node"
    );
    assert_eq!(
        session_end.action_type, "Transfer",
        "session_end trace must carry the last node action type"
    );
    assert_eq!(
        session_end.extra,
        Some(serde_json::json!({"nodetype": "transfer"})),
        "session_end trace must preserve the last node metadata"
    );
    assert_eq!(
        session_end.end_reason,
        Some(crate::call::app::ivr::provider::SessionEndTag::UserHangup),
        "session_end trace must carry the end reason"
    );
    assert_eq!(
        session_end.end,
        Some(true),
        "session_end trace must carry the end marker"
    );
    assert!(
        *provider.end_called.lock().unwrap(),
        "caller hangup is a session end — the provider must receive /end (user_hangup)"
    );
}

#[tokio::test]
async fn test_step_traces_carry_next_pointer_and_end_marker() {
    use crate::call::app::ivr::trace::IvrTraceCollector;

    let trace = IvrTraceCollector::new();

    // Provider-driven menu (empty entries): every digit forwards to the
    // provider. Step 1 = DtmfMenu, step 2 = Transfer (terminal).
    let menu = ActionNode::new(EntryAction::DtmfMenu {
        greeting: Some("menu.wav".into()),
        greeting_text: None,
        greeting_record_list: None,
        greeting_voice: None,
        timeout_ms: 5000,
        max_retries: 3,
        entries: HashMap::new(),
        timeout_action: None,
        invalid_action: None,
        greeting_api_url: None,
    });
    let mut transfer = ActionNode::new(EntryAction::Transfer {
        target: "2001".into(),
        headers: HashMap::new(),
        params: HashMap::new(),
        return_app: None,
        return_target: None,
    });
    transfer.step_id = Some("step-transfer".to_string());

    let mut app: StepIvrApp = mock_app(vec![menu, transfer]);
    app.trace = Some(trace.clone());
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(c, CallCommand::Play { source: crate::call::domain::MediaSource::File { path }, .. } if path == "menu.wav")
        })
        .await;
    stack.audio_complete("ivr_menu_greeting");
    let _ = stack.drain_cmds();
    std::thread::sleep(std::time::Duration::from_millis(50));

    // The digit forwards to the provider, which returns the terminal
    // Transfer node: the menu step's trace must carry the successor's
    // id as its next pointer.
    stack.dtmf("1");
    stack
        .assert_cmd(2000, "stop", |c| {
            matches!(c, CallCommand::StopPlayback { .. })
        })
        .await;
    stack
        .assert_cmd(
            2000,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let entries = trace.query_by_session("test-session").await;

    let menu_exit = entries
        .iter()
        .find(|e| e.trigger.type_name() == "dtmf")
        .expect("menu step trace must be recorded");
    assert_eq!(
        menu_exit.next_step_id.as_deref(),
        Some("step-transfer"),
        "the step's trace must carry the successor's step_id as next_step_id"
    );
    assert_eq!(
        menu_exit.next_node_id.as_deref(),
        Some("step-transfer"),
        "next_node_id mirrors next_step_id in step mode"
    );
    assert_eq!(
        menu_exit.end,
        None,
        "a step the flow continues from must not carry the end marker"
    );

    let transfer_exit = entries
        .iter()
        .find(|e| e.step_id.as_deref() == Some("step-transfer"))
        .expect("terminal transfer step trace must be recorded");
    assert_eq!(
        transfer_exit.end,
        Some(true),
        "a terminal node's trace must carry the end marker"
    );

    let session_end = entries
        .iter()
        .find(|e| e.trigger.type_name() == "session_end")
        .expect("session_end trace must be recorded");
    assert_eq!(
        session_end.end,
        Some(true),
        "the session_end trace must carry the end marker"
    );
}

/// Consumer contract for the [prompt → hangup] natural-completion flow:
/// the FULL trace sequence must reach BOTH the CDR collector and the RWI
/// gateway — prompt step (`session_start`), hangup step (`audio_complete`,
/// `end=true`), and the `session_end` entry (`end=true`, `end_reason`).
/// These three are emitted back-to-back in one burst; the webhook's
/// former same-type same-microsecond dedup collapsed such bursts and
/// silently dropped the flow's end marker, so the exact COUNT is
/// asserted here as the regression guard.
#[tokio::test]
async fn test_prompt_natural_completion_emits_full_trace_sequence() {
    use crate::call::app::ivr::trace::IvrTraceCollector;
    use crate::rwi::gateway::RwiGateway;

    let trace = IvrTraceCollector::new();
    let gateway = RwiGateway::new();
    let mut rwi_events = gateway.subscribe_events();

    let prompt = ActionNode::new(EntryAction::Prompt {
        file: Some("hello.wav".into()),
        tts_text: None,
        tts_voice: None,
        record_name_list: None,
        interruptible: false,
        tts_api_url: None,
        delay_before_ms: 0,
        delay_after_ms: 0,
    });
    let mut app = mock_app(vec![prompt, ActionNode::new(EntryAction::hangup_none())]);
    app.trace = Some(trace.clone());
    app.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gateway)));

    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| play_file_cmd(c, "hello.wav"))
        .await;

    // Prompt finishes naturally → the provider's next node (hangup) runs.
    stack.audio_complete("ivr_prompt");
    stack
        .assert_cmd(2000, "hangup", |c| matches!(c, CallCommand::Hangup { .. }))
        .await;
    // on_exit records the session_end entry; give the spawned collector
    // write a moment to land.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Expected (trigger, end-marker) pairs in emission order.
    let expected: [(&str, Option<bool>); 3] = [
        ("session_start", None),    // prompt step — flow continues
        ("audio_complete", Some(true)), // hangup step — terminal
        ("session_end", Some(true)),    // session close — end_reason carrier
    ];

    // ── CDR collector side ──
    let entries = trace.query_by_session("test-session").await;
    assert_eq!(
        entries.len(),
        3,
        "exactly three trace entries must be recorded, got: {:?}",
        entries.iter().map(|e| e.trigger.type_name()).collect::<Vec<_>>()
    );
    for (entry, (trigger, end)) in entries.iter().zip(expected) {
        assert_eq!(entry.trigger.type_name(), trigger, "trace order mismatch");
        assert_eq!(entry.end, end, "end-marker mismatch on trigger={trigger}");
        assert!(
            entry.step_end_time.is_some(),
            "trigger={trigger} must carry step_end_time (completion marker)"
        );
    }
    let session_end = &entries[2];
    assert_eq!(
        session_end.end_reason,
        Some(crate::call::app::ivr::provider::SessionEndTag::Hangup),
        "session_end entry must carry the end reason"
    );

    // ── RWI fan-out side: the same burst must arrive uncollapsed ──
    let mut rwi_traces: Vec<serde_json::Value> = Vec::new();
    while let Ok(ev) = rwi_events.try_recv() {
        if ev.event.event_type == "ivr_step_trace" {
            rwi_traces.push(ev.event.payload);
        }
    }
    assert_eq!(
        rwi_traces.len(),
        3,
        "all three ivr_step_trace events must be fanned out, got: {rwi_traces:?}"
    );
    for (payload, (trigger, end)) in rwi_traces.iter().zip(expected) {
        assert_eq!(payload["trigger"]["type"], trigger, "fan-out order mismatch");
        assert_eq!(
            payload["end"].as_bool(),
            end,
            "fan-out end-marker mismatch on trigger={trigger}"
        );
    }
    assert!(
        rwi_traces[2]["end_reason"].is_string(),
        "fan-out session_end must carry end_reason"
    );
}

#[tokio::test]
async fn test_step_trace_events_are_enqueued_in_record_order() {
    use crate::rwi::gateway::RwiGateway;

    let mut app = mock_app(vec![]);
    app.sess
        .variables
        .insert("session_id".into(), "test-session".into());
    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    app.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gateway)));

    let entry = |step_id: &str, trigger: crate::rwi::TriggerInfo| IvrTraceEntry {
        session_id: "test-session".into(),
        caller: "1001".into(),
        callee: "2000".into(),
        step_index: 1,
        trigger,
        provider_url: None,
        action_type: "Prompt".into(),
        action_json: None,
        duration_ms: 0,
        error: None,
        step_id: Some(step_id.into()),
        step_name: None,
        step_start_time: None,
        step_end_time: None,
        extra: None,
        end_reason: None,
        end_detail: None,
        next_node_id: None,
        next_step_id: None,
        end: None,
    };

    app.record_trace(entry(
        "step-normal",
        crate::rwi::TriggerInfo::AudioComplete,
    ));
    app.record_trace(entry(
        "step-end",
        crate::rwi::TriggerInfo::SessionEnd,
    ));

    let first = events.try_recv().expect("ordinary trace must be enqueued");
    let second = events
        .try_recv()
        .expect("session end trace must be enqueued");
    assert_eq!(first.event.payload["step_id"], "step-normal");
    assert_eq!(second.event.payload["step_id"], "step-end");
}

/// Completion contract guard: `record_trace` is the single choke point
/// every executor step trace passes through. An entry that reaches it
/// without stamps must be repaired, so an emitted `ivr_step_trace` can
/// NEVER lack `step_end_time` / `step_start_time` (`duration_ms` is
/// non-optional by type). Covers the theoretical `None` site directly.
#[tokio::test]
async fn test_record_trace_backfills_missing_completion_stamps() {
    use crate::rwi::gateway::RwiGateway;

    let mut app = mock_app(vec![]);
    app.sess
        .variables
        .insert("session_id".into(), "test-session".into());
    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    app.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gateway)));

    // Worst case: a site leaves BOTH stamps unset.
    app.record_trace(IvrTraceEntry {
        session_id: "test-session".into(),
        caller: "1001".into(),
        callee: "2000".into(),
        step_index: 1,
        trigger: crate::rwi::TriggerInfo::IvrFallback {
            reason: "test".into(),
            target: None,
        },
        provider_url: None,
        action_type: "ivr_fallback".into(),
        action_json: None,
        duration_ms: 0,
        error: None,
        step_id: None,
        step_name: None,
        step_start_time: None,
        step_end_time: None,
        extra: None,
        end_reason: None,
        end_detail: None,
        next_node_id: None,
        next_step_id: None,
        end: None,
    });

    let ev = events.try_recv().expect("trace must be enqueued");
    let payload = ev.event.payload;
    let start = payload["step_start_time"]
        .as_str()
        .expect("step_start_time must be backfilled");
    let end = payload["step_end_time"]
        .as_str()
        .expect("step_end_time must be backfilled");
    let start_t = chrono::DateTime::parse_from_rfc3339(start).expect("start RFC3339");
    let end_t = chrono::DateTime::parse_from_rfc3339(end).expect("end RFC3339");
    assert!(
        end_t >= start_t,
        "backfilled start/end must satisfy end >= start: {payload}"
    );
    assert!(
        payload["duration_ms"].is_u64(),
        "duration_ms must be present: {payload}"
    );
}

/// Execution-identity contract at the wire choke point: every
/// executor-emitted trace carries THIS runtime instance's
/// `app_execution_id`; without one the key is omitted from the payload
/// entirely (never guessed, never taken from `extra`).
#[tokio::test]
async fn test_record_trace_stamps_app_execution_id() {
    use crate::rwi::gateway::RwiGateway;

    let mk_entry = || IvrTraceEntry {
        session_id: "test-session".into(),
        caller: "1001".into(),
        callee: "2000".into(),
        step_index: 1,
        trigger: crate::rwi::TriggerInfo::IvrFallback {
            reason: "test".into(),
            target: None,
        },
        provider_url: None,
        action_type: "ivr_fallback".into(),
        action_json: None,
        duration_ms: 0,
        error: None,
        step_id: None,
        step_name: None,
        step_start_time: None,
        step_end_time: None,
        extra: None,
        end_reason: None,
        end_detail: None,
        next_node_id: None,
        next_step_id: None,
        end: None,
    };

    // With an identity → carried verbatim on the wire event.
    let mut app = mock_app(vec![]);
    app.sess
        .variables
        .insert("session_id".into(), "test-session".into());
    app.app_execution_id = Some(4);
    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    app.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gateway)));
    app.record_trace(mk_entry());
    let ev = events.try_recv().expect("trace must be enqueued");
    assert_eq!(
        ev.event.payload["app_execution_id"], 4,
        "executor-emitted trace must carry the owning execution's identity"
    );

    // Without an identity → the key is omitted entirely.
    let mut app = mock_app(vec![]);
    app.sess
        .variables
        .insert("session_id".into(), "test-session".into());
    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    app.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gateway)));
    app.record_trace(mk_entry());
    let ev = events.try_recv().expect("trace must be enqueued");
    assert!(
        ev.event.payload.get("app_execution_id").is_none(),
        "unknown identity must be omitted from the payload entirely, got: {}",
        ev.event.payload
    );
}

/// Stash helper contract: the SUSPENDED execution's identity is frozen
/// into shared session vars at resumable hand-off; no identity → no
/// stash (downstream omits the field).
#[test]
fn test_stash_suspended_execution_id() {
    let ctx = make_test_context();

    // With an identity → stashed as a string.
    let mut app = mock_app(vec![]);
    app.runtime_vars = Some(ctx.session_vars.clone());
    app.app_execution_id = Some(3);
    app.stash_suspended_execution_id();
    assert_eq!(
        ctx.get_var(IVR_SUSPENDED_EXECUTION_ID_KEY)
            .as_deref(),
        Some("3"),
        "resumable hand-off must freeze the suspended execution's identity"
    );

    // Without an identity → untouched (never guessed).
    let ctx = make_test_context();
    let mut app = mock_app(vec![]);
    app.runtime_vars = Some(ctx.session_vars.clone());
    app.stash_suspended_execution_id();
    assert!(
        ctx.get_var(IVR_SUSPENDED_EXECUTION_ID_KEY).is_none(),
        "no identity must leave no stash"
    );
}

/// Every step trace emitted by ANY executor flow carries the completion
/// contract: `step_start_time` + `step_end_time` present, `end >= start`,
/// `duration_ms` present and equal to the stamp window (±50ms). Driven
/// through the RWI fan-out (the wire shape consumers see).
#[tokio::test]
async fn test_every_emitted_step_trace_carries_completion_stamps() {
    use crate::rwi::gateway::RwiGateway;

    fn assert_stamps(payloads: &[serde_json::Value], flow: &str) {
        assert!(!payloads.is_empty(), "{flow}: no traces captured");
        for (i, p) in payloads.iter().enumerate() {
            let start = p["step_start_time"].as_str().unwrap_or_else(|| {
                panic!("{flow} trace[{i}] missing step_start_time: {p}")
            });
            let end = p["step_end_time"].as_str().unwrap_or_else(|| {
                panic!("{flow} trace[{i}] missing step_end_time: {p}")
            });
            let duration = p["duration_ms"].as_u64().unwrap_or_else(|| {
                panic!("{flow} trace[{i}] missing duration_ms: {p}")
            });
            let start_t = chrono::DateTime::parse_from_rfc3339(start)
                .unwrap_or_else(|e| panic!("{flow} trace[{i}] start not RFC3339: {e}"));
            let end_t = chrono::DateTime::parse_from_rfc3339(end)
                .unwrap_or_else(|e| panic!("{flow} trace[{i}] end not RFC3339: {e}"));
            assert!(
                end_t >= start_t,
                "{flow} trace[{i}] step_end_time < step_start_time: {p}"
            );
            let expected = (end_t - start_t).num_milliseconds();
            assert!(
                (duration as i64 - expected).abs() <= 50,
                "{flow} trace[{i}] duration_ms {duration} != stamp window {expected}ms: {p}"
            );
        }
    }

    fn drain_traces(
        events: &mut tokio::sync::broadcast::Receiver<
            crate::rwi::gateway::EventCacheEntry,
        >,
    ) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        while let Ok(ev) = events.try_recv() {
            if ev.event.event_type == "ivr_step_trace" {
                out.push(ev.event.payload);
            }
        }
        out
    }

    fn prompt_node(file: &str, interruptible: bool) -> ActionNode {
        ActionNode::new(EntryAction::Prompt {
            file: Some(file.into()),
            tts_text: None,
            tts_voice: None,
            record_name_list: None,
            interruptible,
            tts_api_url: None,
            delay_before_ms: 0,
            delay_after_ms: 0,
        })
    }

    let wire_gateway = |app: &mut StepIvrApp| -> tokio::sync::broadcast::Receiver<crate::rwi::gateway::EventCacheEntry> {
        let gateway = RwiGateway::new();
        let rx = gateway.subscribe_events();
        app.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gateway)));
        rx
    };

    // ── (a) natural completion: [prompt, hangup] ──
    {
        let mut app = mock_app(vec![
            prompt_node("hello.wav", false),
            ActionNode::new(EntryAction::hangup_none()),
        ]);
        let mut events = wire_gateway(&mut app);
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
        stack
            .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(2000, "play", |c| play_file_cmd(c, "hello.wav"))
            .await;
        stack.audio_complete("ivr_prompt");
        stack
            .assert_cmd(2000, "hangup", |c| matches!(c, CallCommand::Hangup { .. }))
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_stamps(&drain_traces(&mut events), "natural completion");
    }

    // ── (b) barge-in on an interruptible prompt ──
    {
        let mut app = mock_app(vec![
            prompt_node("welcome.wav", true),
            ActionNode::new(EntryAction::hangup_none()),
        ]);
        let mut events = wire_gateway(&mut app);
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
        stack
            .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(2000, "play", |c| play_file_cmd(c, "welcome.wav"))
            .await;
        stack.dtmf("5");
        stack
            .assert_cmd(2000, "stop", |c| matches!(c, CallCommand::StopPlayback { .. }))
            .await;
        stack
            .assert_cmd(2000, "hangup", |c| matches!(c, CallCommand::Hangup { .. }))
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_stamps(&drain_traces(&mut events), "barge-in");
    }

    // ── (c) caller hangup mid-prompt (the zero-await on_exit burst) ──
    {
        let mut app = mock_app(vec![prompt_node("long.wav", false)]);
        let mut events = wire_gateway(&mut app);
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
        stack
            .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(2000, "play", |c| play_file_cmd(c, "long.wav"))
            .await;
        stack.remote_hangup();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_stamps(&drain_traces(&mut events), "caller hangup mid-prompt");
    }
}

// ── Invariant: step_end_time implies step_start_time ──
//
// Consumers derive duration_ms as `event timestamp - step_start_time`.
// An entry that carries step_end_time without step_start_time yields an
// empty duration. Every finalized entry (ordinary step, pending finalize,
// dtmf_menu_invalid, ivr_fallback, session_end) must carry both stamps.

#[tokio::test]
async fn test_all_step_traces_with_end_time_carry_start_time() {
    use crate::call::app::ivr::trace::IvrTraceCollector;

    let trace = IvrTraceCollector::new();

    let mut entries = HashMap::new();
    entries.insert(
        "1".into(),
        ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        }),
    );
    let menu = ActionNode::new(EntryAction::DtmfMenu {
        greeting: Some("menu.wav".into()),
        greeting_text: None,
        greeting_record_list: None,
        greeting_voice: None,
        timeout_ms: 5000,
        max_retries: 3,
        entries,
        timeout_action: None,
        invalid_action: None,
        greeting_api_url: None,
    });

    let mut app: StepIvrApp = mock_app(vec![menu]);
    app.trace = Some(trace.clone());
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(c, CallCommand::Play { source: crate::call::domain::MediaSource::File { path }, .. } if path == "menu.wav")
        })
        .await;
    stack.audio_complete("ivr_menu_greeting");
    let _ = stack.drain_cmds();
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Rejected key → dtmf_menu_invalid trace; matching key → pending
    // finalize + terminal transfer step trace; the transfer ends the
    // session and on_exit appends the session_end entry.
    stack.dtmf("9");
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    stack.dtmf("1");
    stack
        .assert_cmd(2000, "stop", |c| {
            matches!(c, CallCommand::StopPlayback { .. })
        })
        .await;
    stack
        .assert_cmd(
            200,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let sessions = trace.sessions().await;
    let sess = &sessions[0];
    let entries = trace.query_by_session(&sess.session_id).await;
    assert!(
        entries
            .iter()
            .any(|e| e.trigger.type_name() == "dtmf_menu_invalid"),
        "expected a dtmf_menu_invalid entry in the trace"
    );
    assert!(
        entries.iter().any(|e| e.trigger.type_name() == "session_end"),
        "expected a session_end entry in the trace"
    );
    for e in &entries {
        assert!(
            !(e.step_end_time.is_some() && e.step_start_time.is_none()),
            "trace entry with trigger '{}' carries step_end_time ({:?}) but no step_start_time — duration_ms would be empty for consumers",
            e.trigger.type_name(),
            e.step_end_time
        );
    }
}

#[tokio::test]
async fn test_rwi_step_trace_events_carry_start_time_when_ended() {
    use crate::rwi::gateway::RwiGateway;

    let (provider, _events_handle) = capturing_menu_first();
    let mut app: StepIvrApp =
        StepIvrApp::with_provider(Box::new(provider)).with_name("start-time-ivr");
    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    app.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gateway)));
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(c, CallCommand::Play { source: crate::call::domain::MediaSource::File { path }, .. } if path == "menu.wav")
        })
        .await;
    stack.audio_complete("ivr_menu_greeting");
    let _ = stack.drain_cmds();
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Caller hangs up while the menu step is still waiting: on_exit
    // finalizes the pending step trace, then records session_end.
    stack.remote_hangup();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let mut ended = 0;
    while let Ok(ev) = events.try_recv() {
        if ev.event.payload["step_end_time"].is_string() {
            ended += 1;
            assert!(
                ev.event.payload["step_start_time"].is_string(),
                "ivr_step_trace event with trigger {:?} carries step_end_time but no step_start_time — duration_ms would be empty for consumers",
                ev.event.payload["trigger"]
            );
        }
    }
    assert!(
        ended >= 2,
        "expected the finalized menu step and the session_end traces, got {ended}"
    );
}

#[tokio::test]
async fn test_fallback_trace_carries_step_start_time() {
    use crate::call::app::ivr::trace::IvrTraceCollector;

    let trace = IvrTraceCollector::new();
    let mut app: StepIvrApp = StepIvrApp::with_provider(Box::new(FailThenEscalateProvider));
    app.trace = Some(trace.clone());
    app.sess
        .variables
        .insert("session_id".into(), "test-session".into());
    app.current_step_start_time = Some("2026-01-01T00:00:00+00:00".to_string());

    // No ivr_fallback configured → `not_configured` trace emitted via
    // record_fallback_trace.
    app.enter_ivr_fallback_node("step:test");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let entries = trace.query_by_session("test-session").await;
    let fallback = entries
        .iter()
        .find(|e| e.action_type == "ivr_fallback")
        .expect("fallback decision must be traced");
    assert_eq!(
        fallback.step_start_time.as_deref(),
        Some("2026-01-01T00:00:00+00:00"),
        "ivr_fallback trace must carry step_start_time — duration_ms would be empty for consumers"
    );
    assert!(
        fallback.step_end_time.is_some(),
        "ivr_fallback trace must carry step_end_time"
    );
    // Contract: duration_ms is derived from the stamps, not hardcoded 0.
    let expected = crate::call::app::ivr::trace::duration_ms_between(
        fallback.step_start_time.as_deref(),
        fallback
            .step_end_time
            .as_deref()
            .expect("fallback must carry step_end_time"),
    );
    assert_eq!(
        fallback.duration_ms, expected,
        "ivr_fallback duration_ms must equal step_end_time - step_start_time"
    );
    assert!(
        fallback.duration_ms > 0,
        "ivr_fallback duration must be derived from its stamps, got 0"
    );
}

// ── Hangup while a step waits: the step's trigger records WHY it ended ──

/// Menu waiting for keys + caller hangs up: the menu step's own trace
/// must carry trigger `user_hangup` (symmetric with dtmf /
/// dtmf_menu_timeout), while the session_end entry still carries the
/// session-level end_reason.
#[tokio::test]
async fn test_menu_hangup_overwrites_pending_trigger() {
    use crate::call::app::ivr::trace::IvrTraceCollector;

    let trace = IvrTraceCollector::new();
    let (provider, _events_handle) = capturing_menu_first();
    let mut app: StepIvrApp =
        StepIvrApp::with_provider(Box::new(provider)).with_name("hangup-trace-ivr");
    app.trace = Some(trace.clone());
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(c, CallCommand::Play { source: crate::call::domain::MediaSource::File { path }, .. } if path == "menu.wav")
        })
        .await;
    stack.audio_complete("ivr_menu_greeting");
    let _ = stack.drain_cmds();
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Caller hangs up while the menu is waiting for keys.
    stack.remote_hangup();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let sessions = trace.sessions().await;
    let sess = &sessions[0];
    let entries = trace.query_by_session(&sess.session_id).await;
    let menu_entry = entries
        .iter()
        .find(|e| e.action_type == "DtmfMenu")
        .expect("expected a DtmfMenu trace entry");
    assert_eq!(
        menu_entry.trigger.type_name(), "user_hangup",
        "menu step terminated by caller hangup must carry trigger 'user_hangup', got: {:?}",
        menu_entry.trigger
    );
    // Session-level end_reason remains on the final session_end entry only.
    let session_end = entries
        .iter()
        .find(|e| e.trigger.type_name() == "session_end")
        .expect("expected a session_end entry");
    assert_eq!(
        session_end.end_reason,
        Some(crate::call::app::ivr::provider::SessionEndTag::UserHangup)
    );
}

/// Same contract for system cancellation: the outstanding wait step is
/// reported with trigger `cancelled`.
#[tokio::test]
async fn test_menu_cancel_overwrites_pending_trigger() {
    use crate::call::app::ivr::trace::IvrTraceCollector;

    let trace = IvrTraceCollector::new();
    let (provider, _events_handle) = capturing_menu_first();
    let mut app: StepIvrApp =
        StepIvrApp::with_provider(Box::new(provider)).with_name("cancel-trace-ivr");
    app.trace = Some(trace.clone());
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(c, CallCommand::Play { source: crate::call::domain::MediaSource::File { path }, .. } if path == "menu.wav")
        })
        .await;
    stack.audio_complete("ivr_menu_greeting");
    let _ = stack.drain_cmds();
    std::thread::sleep(std::time::Duration::from_millis(50));

    stack.cancel();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let sessions = trace.sessions().await;
    let sess = &sessions[0];
    let entries = trace.query_by_session(&sess.session_id).await;
    let menu_entry = entries
        .iter()
        .find(|e| e.action_type == "DtmfMenu")
        .expect("expected a DtmfMenu trace entry");
    assert_eq!(
        menu_entry.trigger.type_name(), "cancelled",
        "menu step terminated by cancellation must carry trigger 'cancelled', got: {:?}",
        menu_entry.trigger
    );
}

#[tokio::test]
async fn test_remote_hangup_emits_step_trace_event() {
    use crate::rwi::gateway::EventCacheEntry;

    let provider = Arc::new(MockProvider::new(vec![ActionNode::new(
        EntryAction::Transfer {
            target: "2001".into(),
            headers: HashMap::new(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        },
    )]));
    let mut app = StepIvrApp::with_provider(Box::new(MockProviderHandle(provider.clone())));
    app.ivr_name = Some("test-ivr".to_string());
    app.current_node = Some(ActionNode::new(EntryAction::Transfer {
        target: "2001".into(),
        headers: HashMap::new(),
        params: HashMap::new(),
        return_app: None,
        return_target: None,
    }));
    app.current_step_id = Some("step-9".to_string());
    app.sess
        .variables
        .insert("session_id".into(), "test-session".into());
    app.sess.variables.insert("caller".into(), "1001".into());
    app.sess.variables.insert("callee".into(), "2000".into());

    let mut gw = crate::rwi::gateway::RwiGateway::new();
    let (tx, mut rx) = tokio::sync::broadcast::channel::<EventCacheEntry>(16);
    gw.set_webhook_tx(tx);
    app.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gw)));

    app.on_exit(crate::call::app::ExitReason::Cancelled)
        .await
        .expect("cancel exit should succeed");

    let mut saw_trace = false;
    let mut saw_end_reason = false;
    for _ in 0..20 {
        while let Ok(entry) = rx.try_recv() {
            if entry.event.event_type == "ivr_step_trace" {
                saw_trace = true;
                if entry.event.payload["end_reason"]
                    .as_str()
                    .map(|r| r == "hangup")
                    .unwrap_or(false)
                {
                    saw_end_reason = true;
                }
            }
        }
        if saw_trace && saw_end_reason {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(saw_trace, "cancel must emit an ivr_step_trace event");
    assert!(
        saw_end_reason,
        "cancel-emitted step trace must carry end_reason"
    );
    assert!(
        !*provider.end_called.lock().unwrap(),
        "cancel must skip provider session end"
    );
}

#[tokio::test]
async fn test_cancel_during_provider_next_skips_session_end() {
    let entered_next = Arc::new(Notify::new());
    let release_next = Arc::new(Notify::new());
    let end_called = Arc::new(AtomicBool::new(false));

    let provider = BlockingProvider {
        entered_next: entered_next.clone(),
        release_next: release_next.clone(),
        end_called: end_called.clone(),
    };
    let ctx = make_test_context();
    let mut stack = MockCallStack::run_with_context(
        Box::new(StepIvrApp::with_provider(Box::new(provider)).with_name("blocking-step-ivr")),
        ctx.clone(),
    );

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;

    tokio::time::timeout(Duration::from_secs(1), entered_next.notified())
        .await
        .expect("provider next_action should start");

    stack.cancel();
    stack.join().await.expect("cancel should stop app");

    assert!(!end_called.load(Ordering::SeqCst), "cancel must skip /end");
    assert_eq!(ctx.get_var(IVR_STATUS_KEY).as_deref(), Some("cancelled"));
    assert_eq!(
        ctx.get_var(IVR_END_REASON_KEY).as_deref(),
        Some("cancelled")
    );

    release_next.notify_waiters();
}

#[tokio::test]
async fn test_startup_failure_sets_runtime_status() {
    let ctx = make_test_context();
    let mut stack = MockCallStack::run_with_context(
        Box::new(StepIvrApp::with_provider(Box::new(FailingProvider)).with_name("failing-ivr")),
        ctx.clone(),
    );

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;

    stack
        .assert_cmd(500, "play fallback", |c| {
            matches!(c, CallCommand::Play { .. })
        })
        .await;
    assert_eq!(
        ctx.get_var(IVR_LAST_ERROR_KEY).as_deref(),
        Some("provider bootstrap failed")
    );
    let status_before_exit = ctx.get_var(IVR_STATUS_KEY);
    assert!(
        matches!(
            status_before_exit.as_deref(),
            Some("startup_error") | Some("active")
        ),
        "unexpected startup status before fallback exit: {status_before_exit:?}"
    );
    stack.audio_complete("ivr_prompt");
    stack
        .assert_cmd(500, "hangup fallback", |c| {
            matches!(c, CallCommand::Hangup(_))
        })
        .await;
    stack
        .join()
        .await
        .expect("fallback path should exit cleanly");

    assert_eq!(ctx.get_var(IVR_STATUS_KEY).as_deref(), Some("hangup"));
    assert_eq!(ctx.get_var(IVR_NAME_KEY).as_deref(), Some("failing-ivr"));
    assert_eq!(ctx.get_var(IVR_END_REASON_KEY).as_deref(), Some("hangup"));
}

#[tokio::test]
async fn test_http_provider_remote_hangup_reports_one_end_webhook() {
    use axum::{Json, Router, extract::State, routing::post};

    #[derive(Default)]
    struct ProviderState {
        start_calls: tokio::sync::Mutex<Vec<serde_json::Value>>,
        step_calls: tokio::sync::Mutex<Vec<serde_json::Value>>,
        end_calls: tokio::sync::Mutex<Vec<serde_json::Value>>,
        step_entered: Notify,
        release_step: Notify,
    }

    async fn start_handler(
        State(state): State<Arc<ProviderState>>,
        Json(body): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        state.start_calls.lock().await.push(body);
        Json(serde_json::json!({ "ok": true }))
    }

    async fn step_handler(
        State(state): State<Arc<ProviderState>>,
        Json(body): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        state.step_calls.lock().await.push(body);
        state.step_entered.notify_waiters();
        state.release_step.notified().await;
        Json(
            serde_json::to_value(ActionNode::new(EntryAction::Transfer {
                target: "2001".into(),
                headers: HashMap::new(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }))
            .unwrap(),
        )
    }

    async fn end_handler(
        State(state): State<Arc<ProviderState>>,
        Json(body): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        state.end_calls.lock().await.push(body);
        Json(serde_json::json!({ "ok": true }))
    }

    let state = Arc::new(ProviderState::default());
    let app = Router::new()
        .route("/ivr/step/start", post(start_handler))
        .route("/ivr/step", post(step_handler))
        .route("/ivr/step/end", post(end_handler))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    crate::utils::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    let provider = StepProvider::new(format!("http://{addr}/ivr/step"), reqwest::Client::new())
        .with_retry(RetryConfig {
            max_retries: 1,
            timeout_ms: 15_000,
            retry_delay_ms: 100,
            fallback_action: None,
        });
    let ctx = make_test_context();
    let mut stack = MockCallStack::run_with_context(
        Box::new(StepIvrApp::with_provider(Box::new(provider)).with_name("http-ivr")),
        ctx,
    );

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;

    tokio::time::timeout(Duration::from_secs(2), state.step_entered.notified())
        .await
        .expect("step provider should receive first /step request");

    // The step handler blocks until released below. A queued remote hangup
    // must terminate the application with one user_hangup /end notification;
    // the event loop owns on_exit and must not emit a second fallback /end.
    stack.remote_hangup();
    stack
        .join()
        .await
        .expect("timeout fallback should stop app");

    assert_eq!(state.start_calls.lock().await.len(), 1);
    assert_eq!(state.step_calls.lock().await.len(), 1);
    let end_calls = state.end_calls.lock().await;
    assert_eq!(end_calls.len(), 1, "remote hangup must emit exactly one /end");
    assert_eq!(end_calls[0]["reason"], "user_hangup");

    state.release_step.notify_waiters();
}

// ── Bug 6: Provider-driven menu timeout forwards to provider ──────────

#[tokio::test]
async fn test_provider_driven_menu_timeout_forwards_to_provider() {
    let menu = ActionNode::new(EntryAction::DtmfMenu {
        greeting: None,
        greeting_text: None,
        greeting_record_list: None,
        greeting_voice: None,
        timeout_ms: 3000,
        max_retries: 1,
        entries: HashMap::new(),
        timeout_action: None,
        invalid_action: None,
        greeting_api_url: None,
    });
    let followup = ActionNode::new(EntryAction::Transfer {
        target: "2001".into(),
        headers: HashMap::new(),
        params: HashMap::new(),
        return_app: None,
        return_target: None,
    });

    let mut stack =
        MockCallStack::run(Box::new(mock_app(vec![menu, followup])), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;

    // Menu has no greeting → NoAudio path → timeout set, app waiting
    tokio::time::sleep(Duration::from_millis(50)).await;
    stack.timeout("ivr_dtmf_timeout");

    // With fix: provider called with DtmfMenuTimeout → returns Transfer
    stack
        .assert_cmd(
            500,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;
}

// ── Bug 7: InputPhone forwards collected digits to provider ───────────

#[tokio::test]
async fn input_phone_emits_completion_before_followup() {
    use crate::rwi::gateway::RwiGateway;

    let mut input_phone = ActionNode::new(EntryAction::InputPhone {
        prompt: Some("enter_phone.wav".into()),
        prompt_text: None,
        prompt_voice: None,
        min_digits: 11,
        max_digits: 11,
        timeout_ms: 10_000,
        inter_digit_timeout_ms: 3_000,
        terminator: "#".into(),
    });
    input_phone.step_id = Some("input-phone-step".into());
    let followup = ActionNode::new(EntryAction::Transfer {
        target: "2001".into(),
        headers: HashMap::new(),
        params: HashMap::new(),
        return_app: None,
        return_target: None,
    });
    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let mut app = mock_app(vec![input_phone, followup]);
    app.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gateway)));

    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path },
                    options: Some(options),
                    ..
                } if path == "enter_phone.wav" && options.interrupt_on_dtmf
            )
        })
        .await;

    // Inject DTMF digits while collect_dtmf is waiting
    stack.dtmf("12345678901");

    // With fix: provider called with PhoneCollected → returns Transfer
    stack
        .assert_cmd(
            500,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    // Single finalized trace: original phone_collected trigger, end time
    // filled, no wait_finalized duplicate.
    let finalized = events
        .try_recv()
        .expect("input phone trace must be enqueued");
    assert_eq!(finalized.event.payload["step_id"], "input-phone-step");
    assert_eq!(
        finalized.event.payload["trigger"]["type"],
        "phone_collected"
    );
    assert_eq!(
        finalized.event.payload["trigger"]["detail"]["number"],
        "12345678901"
    );
    assert!(finalized.event.payload["step_end_time"].is_string());
    // Subsequent events are legitimate (transfer step trace, session_end) —
    // none may be a wait_finalized duplicate.
    while let Ok(ev) = events.try_recv() {
        assert_ne!(
            ev.event.payload["trigger"]["type"], "wait_finalized",
            "input phone completion must not emit a wait_finalized duplicate"
        );
    }
}

#[tokio::test]
async fn typed_result_discards_stale_buffered_dtmf() {
    let (provider, events) =
        CapturingProvider::scripted(|_, _| Ok(test_transfer_node()));
    let mut app = StepIvrApp::with_provider(Box::new(provider));
    app.pending_dtmf.push_back("1".to_string());

    app.request_next(Some(ProviderEvent::PhoneCollected {
        number: "10000000000".to_string(),
    }))
    .await
    .unwrap();

    assert!(app.pending_dtmf.is_empty());
    assert!(matches!(
        events.lock().unwrap().as_slice(),
        [Some(ProviderEvent::PhoneCollected { number })] if number == "10000000000"
    ));
}

// ── Bug 8: Torecord forwards recording complete to provider ──────────

#[tokio::test]
async fn test_torecord_recording_complete_forwards_to_provider() {
    let torecord = ActionNode::new(EntryAction::Torecord {
        prompt: Some("record.wav".into()),
        beep: true,
        max_duration_secs: Some(5),
    });
    let followup = ActionNode::new(EntryAction::Transfer {
        target: "2001".into(),
        headers: HashMap::new(),
        params: HashMap::new(),
        return_app: None,
        return_target: None,
    });

    let mut stack =
        MockCallStack::run(Box::new(mock_app(vec![torecord, followup])), "1001", "2000");
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path }, ..
                } if path == "record.wav"
            )
        })
        .await;
    stack
        .assert_cmd(2000, "start_record", |c| {
            matches!(c, CallCommand::StartRecording { .. })
        })
        .await;

    // Inject recording complete
    tokio::time::sleep(Duration::from_millis(50)).await;
    stack.record_complete("recordings/test.wav", Duration::from_secs(3), 12345);

    // With fix: provider called with RecordingComplete → returns Transfer
    stack
        .assert_cmd(
            500,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;
}

// ── Mid-call record_start / record_stop ──────────────────────────────

#[tokio::test]
async fn test_record_start_stop_continues_without_waiting() {
    let start = ActionNode::new(EntryAction::RecordStart {
        segment_type: Some("ivr".into()),
        id: Some("seg1".into()),
        beep: false,
        max_duration_secs: None,
    });
    let stop = ActionNode::new(EntryAction::RecordStop {
        reason: Some("before_transfer".into()),
    });
    let followup = ActionNode::new(EntryAction::Transfer {
        target: "2001".into(),
        headers: HashMap::new(),
        params: HashMap::new(),
        return_app: None,
        return_target: None,
    });

    let mut stack = MockCallStack::run(
        Box::new(mock_app(vec![start, stop, followup])),
        "1001",
        "2000",
    );
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "start_record", |c| {
            matches!(
                c,
                CallCommand::StartRecording { config }
                    if config.segment_type.as_deref() == Some("ivr")
                        && config.segment_id.as_deref() == Some("seg1")
                        && config.notify_app == Some(false)
                        && config.path.is_empty()
            )
        })
        .await;
    stack
        .assert_cmd(2000, "stop_record", |c| {
            matches!(c, CallCommand::StopRecording)
        })
        .await;
    stack
        .assert_cmd(
            500,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;
}

#[tokio::test]
async fn test_segment_recording_complete_does_not_hijack_flow() {
    // record_start then prompt; a late RecordingComplete must not replace
    // the prompt with a provider RecordingComplete branch.
    let start = ActionNode::new(EntryAction::RecordStart {
        segment_type: Some("ivr".into()),
        id: Some("s1".into()),
        beep: false,
        max_duration_secs: None,
    });
    let prompt = ActionNode::new(EntryAction::Prompt {
        file: Some("hello.wav".into()),
        tts_text: None,
        tts_voice: None,
        record_name_list: None,
        interruptible: false,
        tts_api_url: None,

        delay_before_ms: 0,
        delay_after_ms: 0,
    });
    let hangup = ActionNode::new(EntryAction::hangup_none());

    let mut stack = MockCallStack::run(
        Box::new(mock_app(vec![start, prompt, hangup])),
        "1001",
        "2000",
    );
    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "start_record", |c| {
            matches!(c, CallCommand::StartRecording { .. })
        })
        .await;
    stack
        .assert_cmd(2000, "play", |c| {
            matches!(
                c,
                CallCommand::Play {
                    source: crate::call::domain::MediaSource::File { path },
                    ..
                } if path == "hello.wav"
            )
        })
        .await;

    // Spurious RecordingComplete while waiting for audio — must be ignored
    // (pending action is Prompt, not Torecord).
    stack.record_complete("/tmp/seg.wav", Duration::from_secs(1), 100);
    tokio::time::sleep(Duration::from_millis(30)).await;

    stack.audio_complete("ivr_prompt");
    stack
        .assert_cmd(500, "hangup", |c| matches!(c, CallCommand::Hangup(_)))
        .await;
}

// ── StepProvider ↔ HTTP contract test ────────────────────────────────
//
// Exercises the StepProvider HTTP client end-to-end against an in-process
// mock provider (no external Python/example dependency): request bodies are
// POSTed to /ivr/step, responses parsed into ActionNodes, and
// session_start/end notifications are delivered.

/// Minimal in-process step provider. Accepts keep-alive HTTP/1.1
/// connections and answers `/ivr/step` based on the event type — mirroring
/// what a real step provider (e.g. `examples/unified_ivr_provider.py`)
/// returns.
struct MockStepProviderServer {
    url: String,
    requests: Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
    _listener: std::net::TcpListener,
}

fn start_mock_step_provider(start_action: &'static [u8]) -> MockStepProviderServer {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock step provider");
    let url = format!(
        "http://{}/ivr/step",
        listener.local_addr().expect("mock provider addr")
    );
    let accept_listener = listener.try_clone().expect("clone mock provider listener");
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded_requests = requests.clone();
    std::thread::spawn(move || {
        for stream in accept_listener.incoming() {
            let Ok(stream) = stream else { continue };
            let connection_requests = recorded_requests.clone();
            std::thread::spawn(move || {
                let _ = serve_connection(stream, connection_requests, start_action);
            });
        }
    });
    MockStepProviderServer {
        url,
        requests,
        _listener: listener,
    }
}

fn serve_connection(
    mut stream: std::net::TcpStream,
    requests: Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
    start_action: &[u8],
) -> std::io::Result<()> {
    use std::io::{BufRead, BufReader, Read, Write};

    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    loop {
        // Request line: `POST /ivr/step HTTP/1.1`
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
        // Headers
        let mut content_length = 0usize;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header)? == 0 {
                return Ok(());
            }
            if header.trim().is_empty() {
                break;
            }
            // HTTP header names are case-insensitive; reqwest sends
            // `content-length:` lowercase. A case-sensitive match here
            // mis-reads every request body and desyncs the keep-alive
            // connection (bodies leak into the next request line).
            let h = header.trim_start();
            if h.len() >= 15 && h[..15].eq_ignore_ascii_case("content-length:") {
                content_length = h[15..].trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body)?;
        if let Ok(value) = serde_json::from_slice(&body) {
            requests.lock().unwrap().push((path.clone(), value));
        }
        let payload = mock_step_response(&path, &body, start_action);
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: keep-alive\r\n\r\n",
            payload.len()
        );
        stream.write_all(head.as_bytes())?;
        stream.write_all(&payload)?;
        stream.flush()?;
    }
}

fn mock_step_response(path: &str, body: &[u8], start_action: &[u8]) -> Vec<u8> {
    if path != "/ivr/step" {
        // /ivr/step/start and /ivr/step/end are fire-and-forget.
        return br#"{"status":"ok"}"#.to_vec();
    }
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) {
        let event_type = value["event"]["type"].as_str().unwrap_or("");
        if event_type == "session_start" {
            return start_action.to_vec();
        }
        if event_type == "dtmf" && value["event"]["digit"].as_str() == Some("2") {
            return br#"{"type":"queue","target":"sales"}"#.to_vec();
        }
    }
    br#"{"type":"hangup"}"#.to_vec()
}

#[tokio::test]
async fn test_step_provider_http_contract() {
    use crate::call::app::ivr::provider::StepProvider;

    let provider = start_mock_step_provider(
        br#"{"type":"prompt","tts_text":"IVR step announcement.","interruptible":false,"ignore_prompt_dtmf":true}"#,
    );
    let step_provider = StepProvider::new(&provider.url, reqwest::Client::new());

    let session = SessionContext {
        session_id: "test-session".to_string(),
        app_execution_id: 2,
        caller: "1001".to_string(),
        callee: "2000".to_string(),
        direction: "inbound".to_string(),
        tenant_id: None,
        ivr_id: None,
        variables: HashMap::from([("order_id".to_string(), "order-001".to_string())]),
        sip_headers: Some(HashMap::from([(
            "X-Business-Type".to_string(),
            "34".to_string(),
        )])),
        route_name: None,
        custom_data: None,
        transferred_from: None,
    };
    step_provider.on_session_start(&session).await.unwrap();

    let ctx = ProviderContext {
        session_id: session.session_id.clone(),
        app_execution_id: session.app_execution_id,
        caller: session.caller.clone(),
        callee: session.callee.clone(),
        direction: session.direction.clone(),
        tenant_id: None,
        ivr_id: None,
        variables: HashMap::new(),
        sip_headers: None,
        event: Some(ProviderEvent::SessionStart),
        route_name: None,
        custom_data: None,
        step_start_time: None,
        step_end_time: None,
        step_duration_ms: None,
        step_index: None,
        transferred_from: None,
    };
    let prompt = step_provider.next_action(ctx).await.unwrap();
    assert!(
        matches!(prompt.action, EntryAction::Prompt { ref tts_text, interruptible: false, .. }
            if tts_text.as_deref().is_some_and(|text| text.contains("IVR step")))
    );
    assert!(prompt.ignore_prompt_dtmf);

    let ctx = ProviderContext {
        event: Some(ProviderEvent::Dtmf {
            digit: "2".to_string(),
        }),
        ..ProviderContext {
            session_id: session.session_id.clone(),
            app_execution_id: session.app_execution_id,
            caller: session.caller.clone(),
            callee: session.callee.clone(),
            direction: session.direction.clone(),
            tenant_id: None,
            ivr_id: None,
            variables: HashMap::new(),
            sip_headers: None,
            event: None,
            route_name: None,
            custom_data: None,
            step_start_time: None,
            step_end_time: None,
            step_duration_ms: None,
            step_index: None,
            transferred_from: None,
        }
    };
    let action = step_provider.next_action(ctx).await.unwrap();
    // The mock maps DTMF "2" → queue "sales", matching the documented
    // step-provider menu flow (see examples/unified_ivr_provider.py).
    assert!(
        matches!(action.action, EntryAction::Queue { ref target, .. } if target == "sales")
    );

    step_provider
        .on_session_end_context(
            &SessionEndReason {
                reason: SessionEndTag::Normal,
                detail: None,
            },
            &session,
        )
        .await
        .unwrap();

    let requests = provider.requests.lock().unwrap();
    let start = requests
        .iter()
        .find(|(path, _)| path == "/ivr/step/start")
        .map(|(_, body)| body)
        .expect("start request");
    assert_eq!(start["session_id"], "test-session");
    assert_eq!(start["app_execution_id"], 2);
    assert_eq!(start["variables"]["order_id"], "order-001");
    assert_eq!(start["sip_headers"]["X-Business-Type"], "34");

    let step_requests = requests
        .iter()
        .filter(|(path, _)| path == "/ivr/step")
        .map(|(_, body)| body)
        .collect::<Vec<_>>();
    assert_eq!(step_requests.len(), 2);
    assert!(
        step_requests.iter().all(|body| {
            body["session_id"] == "test-session" && body["app_execution_id"] == 2
        })
    );

    let end = requests
        .iter()
        .find(|(path, _)| path == "/ivr/step/end")
        .map(|(_, body)| body)
        .expect("end request");
    assert_eq!(end["session_id"], "test-session");
    assert_eq!(end["app_execution_id"], 2);
}

#[tokio::test]
async fn runtime_stop_preserves_provider_hangup_notification() {
    use crate::call::app::ivr::provider::StepProvider;
    use crate::call::runtime::{AppFactory, AppRuntime, AppRuntimeConfig, DefaultAppRuntime};
    use crate::proxy::proxy_call::sip_session::SipSessionHandle;

    struct Factory(String);
    #[async_trait]
    impl AppFactory for Factory {
        async fn create_app(
            &self,
            _name: &str,
            _params: Option<serde_json::Value>,
            _context: &ApplicationContext,
        ) -> Result<Option<Box<dyn CallApp>>, anyhow::Error> {
            Ok(Some(Box::new(StepIvrApp::with_provider(Box::new(
                StepProvider::new(&self.0, reqwest::Client::new()),
            )))))
        }
    }

    let mut failures = Vec::new();
    let scenarios = [
        ("caller_hangup", true),
        ("shutdown", true),
        ("text_hangup", true),
    ]
    .into_iter()
    .chain(std::iter::repeat_n(("early_hangup", false), 16));
    for (reason, ready) in scenarios {
        let server = start_mock_step_provider(
            br#"{"type":"prompt","file":"hello.wav","interruptible":false}"#,
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let runtime = DefaultAppRuntime::new(AppRuntimeConfig {
            session_id: "test-session".into(),
            handle: SipSessionHandle::new_for_test("test-session", tx),
            context: Arc::new(make_test_context()),
        })
        .with_factory(Arc::new(Factory(server.url.clone())));
        runtime.start_app("ivr", None, false).await.unwrap();
        // Immediate cancellation exercises startup before the spawned app is polled.
        // Ready scenarios retain the real provider-start/play boundary.
        while ready {
            let command = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("IVR must become ready")
                .expect("command channel must remain open");
            if matches!(command, CallCommand::FromApplication { ref command }
                if matches!(command.as_ref(), CallCommand::Play { .. }))
            {
                break;
            }
        }
        let stop_reason = if reason == "text_hangup" || reason == "early_hangup" {
            "caller_hangup"
        } else {
            reason
        };
        if reason == "caller_hangup" || reason == "early_hangup" {
            runtime
                .stop_app_with_exit_reason(
                    Some(stop_reason.into()),
                    crate::call::app::ExitReason::RemoteHangup(None),
                )
                .await
                .unwrap();
        } else {
            runtime.stop_app(Some(stop_reason.into())).await.unwrap();
        }
        loop {
            let command = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("IVR must exit")
                .expect("command channel must remain open");
            if matches!(command, CallCommand::AppExited { .. }) {
                break;
            }
        }
        let requests = server.requests.lock().unwrap();
        let ends: Vec<_> = requests
            .iter()
            .filter(|(path, _)| path == "/ivr/step/end")
            .collect();
        if reason == "early_hangup" {
            for (_, end) in &ends {
                if end["session_id"] != "test-session" || end["app_execution_id"] != 1 {
                    failures.push(format!(
                        "startup cancellation sent an unidentified end: {end}"
                    ));
                }
            }
        } else if reason == "text_hangup" {
            if !ends.is_empty() {
                failures.push("public stop text must not become a user hangup".into());
            }
        } else if reason == "caller_hangup" {
            assert_eq!(
                ends.len(),
                1,
                "caller BYE must finalize the provider session exactly once"
            );
            assert_eq!(ends[0].1["session_id"], "test-session");
            assert_eq!(ends[0].1["app_execution_id"], 1);
            assert_eq!(ends[0].1["reason"], "user_hangup");
        } else {
            assert!(
                ends.is_empty(),
                "system shutdown must retain cancellation semantics"
            );
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ── DTMF delivery in step-provider mode ──────────────────────────────
//
// DTMF delivery must preserve the current prompt's interruption contract.

fn event_label(ev: &Option<ProviderEvent>) -> String {
    match ev {
        Some(ProviderEvent::SessionStart) => "session_start".to_string(),
        Some(ProviderEvent::AudioComplete { .. }) => "audio_complete".to_string(),
        Some(ProviderEvent::Dtmf { digit }) => format!("dtmf:{digit}"),
        Some(ProviderEvent::DtmfMenuTimeout) => "dtmf_menu_timeout".to_string(),
        Some(ProviderEvent::Error { .. }) => "error".to_string(),
        _ => "other".to_string(),
    }
}

/// Provider that records every event it is called with, and answers:
/// session_start → interruptible welcome; audio_complete → non-interruptible
/// announcement; dtmf "2" → transfer to 2001; anything else → hangup.
struct ScriptedProvider {
    log: Arc<std::sync::Mutex<Vec<String>>>,
    ignore_prompt_dtmf: bool,
}

#[async_trait]
impl ActionProvider for ScriptedProvider {
    async fn next_action(&self, ctx: ProviderContext) -> anyhow::Result<ActionNode> {
        self.log.lock().unwrap().push(event_label(&ctx.event));
        match &ctx.event {
            Some(ProviderEvent::SessionStart) => Ok(ActionNode::new(EntryAction::Prompt {
                file: Some("welcome.wav".into()),
                tts_text: None,
                tts_voice: None,
                record_name_list: None,
                interruptible: true,
                tts_api_url: None,

                delay_before_ms: 0,
                delay_after_ms: 0,
            })),
            Some(ProviderEvent::AudioComplete { .. }) => {
                let mut node = ActionNode::new(EntryAction::Prompt {
                    file: Some("announce.wav".into()),
                    tts_text: None,
                    tts_voice: None,
                    record_name_list: None,
                    interruptible: false,
                    tts_api_url: None,

                    delay_before_ms: 0,
                    delay_after_ms: 0,
                });
                node.ignore_prompt_dtmf = self.ignore_prompt_dtmf;
                Ok(node)
            }
            Some(ProviderEvent::Dtmf { digit }) if digit == "2" => {
                Ok(ActionNode::new(EntryAction::Transfer {
                    target: "2001".into(),
                    headers: HashMap::new(),
                    params: HashMap::new(),
                    return_app: None,
                    return_target: None,
                }))
            }
            _ => Ok(ActionNode::new(EntryAction::hangup_none())),
        }
    }

    async fn on_session_start(&self, _ctx: &SessionContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn on_session_end(
        &self,
        _reason: &SessionEndReason,
        _session_id: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

fn play_file_cmd(cmd: &CallCommand, path: &str) -> bool {
    matches!(
        cmd,
        CallCommand::Play {
            source: crate::call::domain::MediaSource::File { path: p }, ..
        } if p == path
    )
}

#[tokio::test]
async fn test_step_provider_buffers_dtmf_by_default_during_non_interruptible_prompt() {
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let app = StepIvrApp::with_provider(Box::new(ScriptedProvider {
        log: log.clone(),
        ignore_prompt_dtmf: false,
    }));
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(500, "play-welcome", |c| play_file_cmd(c, "welcome.wav"))
        .await;
    stack.audio_complete("ivr_prompt");
    stack
        .assert_cmd(500, "play-announce", |c| play_file_cmd(c, "announce.wav"))
        .await;

    stack.dtmf("2");
    stack.audio_complete("ivr_prompt");
    stack
        .assert_cmd(
            500,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    let log = log.lock().unwrap();
    assert_eq!(
        log.as_slice(),
        ["session_start", "audio_complete", "dtmf:2"]
    );
}

#[tokio::test]
async fn test_step_provider_ignores_dtmf_during_non_interruptible_prompt() {
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let app = StepIvrApp::with_provider(Box::new(ScriptedProvider {
        log: log.clone(),
        ignore_prompt_dtmf: true,
    }));
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(500, "play-welcome", |c| play_file_cmd(c, "welcome.wav"))
        .await;

    // welcome (interruptible) completes → announcement (non-interruptible).
    stack.audio_complete("ivr_prompt");
    stack
        .assert_cmd(500, "play-announce", |c| play_file_cmd(c, "announce.wav"))
        .await;

    // Press 2 during the non-interruptible announcement. The announcement
    // must finish naturally without forwarding the digit to the provider.
    stack.dtmf("2");

    // Announcement completion remains an audio_complete event, so the
    // provider returns its next announcement instead of the DTMF transfer.
    stack.audio_complete("ivr_prompt");
    stack
        .assert_cmd(500, "play-next-announce", |c| {
            play_file_cmd(c, "announce.wav")
        })
        .await;

    let log = log.lock().unwrap();
    assert_eq!(
        log.as_slice(),
        ["session_start", "audio_complete", "audio_complete"]
    );
}

#[tokio::test]
async fn test_step_provider_barges_in_interruptible_prompt() {
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let app = StepIvrApp::with_provider(Box::new(ScriptedProvider {
        log: log.clone(),
        ignore_prompt_dtmf: false,
    }));
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(500, "play-welcome", |c| play_file_cmd(c, "welcome.wav"))
        .await;

    // Press 2 during the interruptible welcome → immediate barge-in.
    stack.dtmf("2");
    stack
        .assert_cmd(500, "stop", |c| {
            matches!(c, CallCommand::StopPlayback { .. })
        })
        .await;
    stack
        .assert_cmd(
            500,
            "transfer",
            |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
        )
        .await;

    let log = log.lock().unwrap();
    assert!(
        log.iter().any(|e| e == "dtmf:2"),
        "provider never received dtmf:2 — got {log:?}"
    );
}

// ── Runaway-loop guard ────────────────────────────────────────────────

/// Provider that answers every event with the same interruptible prompt,
/// simulating a broken/looping provider that never terminates.
struct LoopingProvider;

#[async_trait]
impl ActionProvider for LoopingProvider {
    async fn next_action(&self, _ctx: ProviderContext) -> anyhow::Result<ActionNode> {
        Ok(ActionNode::new(EntryAction::Prompt {
            file: Some("menu.wav".into()),
            tts_text: None,
            tts_voice: None,
            record_name_list: None,
            interruptible: true,
            tts_api_url: None,

            delay_before_ms: 0,
            delay_after_ms: 0,
        }))
    }

    async fn on_session_start(&self, _ctx: &SessionContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn on_session_end(
        &self,
        _reason: &SessionEndReason,
        _session_id: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn test_step_provider_runaway_loop_guard_hangs_up() {
    let app = StepIvrApp::with_provider(Box::new(LoopingProvider)).with_max_repeat_prompts(2);
    let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

    stack
        .assert_cmd(2000, "accept", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(500, "play-menu", |c| play_file_cmd(c, "menu.wav"))
        .await;

    // Cycle 1..2: no input → still re-offered by the provider.
    stack.audio_complete("ivr_prompt");
    stack
        .assert_cmd(500, "play-menu-2", |c| play_file_cmd(c, "menu.wav"))
        .await;
    stack.audio_complete("ivr_prompt");
    stack
        .assert_cmd(500, "play-menu-3", |c| play_file_cmd(c, "menu.wav"))
        .await;

    // Cycle 3: probe with DtmfMenuTimeout; a looping provider ignores it.
    stack.audio_complete("ivr_prompt");
    stack
        .assert_cmd(500, "play-menu-4", |c| play_file_cmd(c, "menu.wav"))
        .await;

    // Cycle 4: probe already ignored → hang up.
    stack.audio_complete("ivr_prompt");
    stack
        .assert_cmd(500, "hangup", |c| matches!(c, CallCommand::Hangup { .. }))
        .await;
}

struct FailRecoveryProvider {
    fail_called: Arc<std::sync::Mutex<u32>>,
}

#[async_trait]
impl ActionProvider for FailRecoveryProvider {
    async fn next_action(&self, _ctx: ProviderContext) -> anyhow::Result<ActionNode> {
        Ok(ActionNode::new(EntryAction::Prompt {
            file: Some("welcome.wav".into()),
            tts_text: None,
            tts_voice: None,
            record_name_list: None,
            interruptible: false,
            tts_api_url: None,

            delay_before_ms: 0,
            delay_after_ms: 0,
        }))
    }

    async fn fail_action(&self, ctx: ProviderContext) -> anyhow::Result<ActionNode> {
        *self.fail_called.lock().unwrap() += 1;
        assert!(
            matches!(ctx.event, Some(ProviderEvent::Fail { .. })),
            "fail_action must receive Fail event"
        );
        Ok(ActionNode::new(EntryAction::Hangup {
            prompt: Some("sounds/recovered.wav".into()),
            prompt_text: None,
            prompt_voice: None,

            delay_before_ms: 0,
            delay_after_ms: 0,
        }))
    }
}

#[tokio::test]
async fn test_execute_failure_calls_fail_then_continues() {
    let fail_called = Arc::new(std::sync::Mutex::new(0u32));
    let mut app = StepIvrApp::with_provider(Box::new(FailRecoveryProvider {
        fail_called: fail_called.clone(),
    }));
    app.sess.variables.insert("session_id".into(), "s1".into());
    app.sess.variables.insert("caller".into(), "1001".into());
    app.sess.variables.insert("callee".into(), "4000".into());

    let node = app
        .recover_from_execute_failure(anyhow::anyhow!("transfer start failed"))
        .await
        .expect("fail recovery");
    assert_eq!(*fail_called.lock().unwrap(), 1);
    match node.action {
        EntryAction::Hangup {
            prompt: Some(p), ..
        } => assert_eq!(p, "sounds/recovered.wav"),
        other => panic!("expected recovered hangup, got {other:?}"),
    }
}

struct FailThenEscalateProvider;

#[async_trait]
impl ActionProvider for FailThenEscalateProvider {
    async fn next_action(&self, _ctx: ProviderContext) -> anyhow::Result<ActionNode> {
        Err(anyhow::anyhow!("step unreachable"))
    }

    async fn fail_action(&self, _ctx: ProviderContext) -> anyhow::Result<ActionNode> {
        Err(anyhow::anyhow!("fail unreachable"))
    }
}

#[tokio::test]
async fn test_step_fail_with_ivr_fallback_jumps_to_target() {
    let fb = Arc::new(crate::config::IvrFallbackConfig {
        default: Some("default_ivr".into()),
        rules: vec![crate::config::IvrFallbackRule {
            name: Some("vip".into()),
            priority: 10,
            match_conditions: crate::proxy::routing::MatchConditions {
                from_user: Some("1001".into()),
                ..Default::default()
            },
            target: "builtin_vip".into(),
        }],
    });
    let mut app = StepIvrApp::with_provider(Box::new(FailThenEscalateProvider))
        .with_ivr_fallback(Some(fb));
    app.sess.variables.insert("session_id".into(), "s1".into());
    app.sess.variables.insert("caller".into(), "1001".into());
    app.sess.variables.insert("callee".into(), "4000".into());

    let node = app
        .request_next(Some(ProviderEvent::SessionStart))
        .await
        .expect("fallback node");
    match node.action {
        EntryAction::Transfer { target, params, .. } => {
            assert_eq!(target, "ivr:builtin_vip");
            assert_eq!(
                params
                    .get(crate::call::app::ivr::fallback::IVR_FALLBACK_USED_KEY)
                    .map(String::as_str),
                Some("1")
            );
        }
        other => panic!("expected direct IVR fallback, got {other:?}"),
    }
    assert!(app.fallback_already_used());
}

#[tokio::test]
async fn test_ivr_fallback_uses_default_when_no_rule() {
    let fb = Arc::new(crate::config::IvrFallbackConfig {
        default: Some("default_ivr".into()),
        rules: vec![crate::config::IvrFallbackRule {
            name: Some("vip".into()),
            priority: 10,
            match_conditions: crate::proxy::routing::MatchConditions {
                from_user: Some("^9".into()),
                ..Default::default()
            },
            target: "vip_ivr".into(),
        }],
    });
    let mut app = StepIvrApp::with_provider(Box::new(FailThenEscalateProvider))
        .with_ivr_fallback(Some(fb));
    app.sess.variables.insert("caller".into(), "1001".into());
    app.sess.variables.insert("callee".into(), "4000".into());

    let node = app.enter_ivr_fallback_node("step:test");
    match node.action {
        EntryAction::Transfer { target, .. } => assert_eq!(target, "ivr:default_ivr"),
        other => panic!("expected default direct IVR transfer, got {other:?}"),
    }
}

#[tokio::test]
async fn test_ivr_fallback_rejected_when_already_used() {
    let fb = Arc::new(crate::config::IvrFallbackConfig {
        default: Some("default_ivr".into()),
        rules: vec![],
    });
    let mut app = StepIvrApp::with_provider(Box::new(FailThenEscalateProvider))
        .with_ivr_fallback(Some(fb));
    app.mark_fallback_used();
    let node = app.enter_ivr_fallback_node("again");
    match node.action {
        EntryAction::Prompt { file: Some(f), .. } => assert_eq!(f, "sounds/error.wav"),
        other => panic!("expected error hangup chain, got {other:?}"),
    }
}

// ── Re-entry loop guard ──────────────────────────────────────────────────

fn reentry_app(name: &str, max_restarts: u32) -> StepIvrApp {
    StepIvrApp::with_provider(Box::new(MockProvider::new(vec![])))
        .with_name(name)
        .with_max_flow_restarts(max_restarts)
}

#[tokio::test]
async fn test_step_ivr_reentry_guard_triggers_after_threshold() {
    let ctx = make_test_context();
    let mut app = reentry_app("main", 3);
    for cycle in 1..=3 {
        assert!(
            app.check_reentry_loop(&ctx, false).is_none(),
            "entry {cycle} within the threshold must pass"
        );
    }
    let node = app
        .check_reentry_loop(&ctx, false)
        .expect("entry beyond the threshold must trigger the guard");
    // No ivr_fallback configured → error-tone + hangup chain.
    match node.action {
        EntryAction::Prompt { file: Some(f), .. } => assert_eq!(f, "sounds/error.wav"),
        other => panic!("expected error-tone hangup chain, got {other:?}"),
    }
}

#[tokio::test]
async fn test_step_ivr_reentry_guard_resets_outside_window() {
    let ctx = make_test_context();
    let mut app = reentry_app("main", 3);
    for _ in 0..3 {
        assert!(app.check_reentry_loop(&ctx, false).is_none());
    }
    // Backdate the last entry beyond the window — the next entry is a
    // fresh, legitimate one (counter resets to 1).
    let backdated = serde_json::json!({
        "main": {
            "count": 3,
            "ts": (chrono::Utc::now() - chrono::Duration::seconds(120)).to_rfc3339(),
        }
    });
    ctx.session_vars
        .insert(IVR_REENTRY_STATE_KEY.to_string(), backdated.to_string());
    assert!(
        app.check_reentry_loop(&ctx, false).is_none(),
        "an entry outside the window must reset the counter"
    );
    assert!(app.check_reentry_loop(&ctx, false).is_none());
    assert!(app.check_reentry_loop(&ctx, false).is_none());
    assert!(
        app.check_reentry_loop(&ctx, false).is_some(),
        "guard must trigger again after another run of in-window entries"
    );
}

#[tokio::test]
async fn test_step_ivr_reentry_guard_counts_per_ivr_identity() {
    let ctx = make_test_context();
    let mut a = reentry_app("ivr-a", 3);
    let mut b = reentry_app("ivr-b", 3);
    for _ in 0..3 {
        assert!(a.check_reentry_loop(&ctx, false).is_none());
    }
    // A different flow is not affected by ivr-a's counter.
    assert!(b.check_reentry_loop(&ctx, false).is_none());
    assert!(
        a.check_reentry_loop(&ctx, false).is_some(),
        "ivr-a must trigger on its own threshold breach"
    );
}

#[tokio::test]
async fn test_step_ivr_reentry_guard_disabled_with_zero() {
    let ctx = make_test_context();
    let mut app = reentry_app("main", 0);
    for _ in 0..10 {
        assert!(
            app.check_reentry_loop(&ctx, false).is_none(),
            "max_flow_restarts=0 must disable the guard"
        );
    }
    assert!(ctx.session_vars.get(IVR_REENTRY_STATE_KEY).is_none());
}

#[tokio::test]
async fn test_step_ivr_reentry_guard_cleared_by_dtmf() {
    let ctx = make_test_context();
    let mut app = reentry_app("main", 3);
    assert!(app.check_reentry_loop(&ctx, false).is_none());
    assert!(app.check_reentry_loop(&ctx, false).is_none());
    // Real caller input proves the flow is progressing — the guard resets.
    app.clear_reentry_state(&ctx);
    assert!(ctx.session_vars.get(IVR_REENTRY_STATE_KEY).is_none());
    assert!(app.check_reentry_loop(&ctx, false).is_none());
}

#[tokio::test]
async fn test_step_ivr_reentry_guard_diverts_to_ivr_fallback_once() {
    let ctx = make_test_context();
    let mk_fb = || {
        Arc::new(crate::config::IvrFallbackConfig {
            default: Some("default_ivr".into()),
            rules: vec![],
        })
    };
    let mut app = StepIvrApp::with_provider(Box::new(MockProvider::new(vec![])))
        .with_name("main")
        .with_max_flow_restarts(1)
        .with_ivr_fallback(Some(mk_fb()));
    // on_enter shares the session-vars map with the app instance.
    app.runtime_vars = Some(ctx.session_vars.clone());

    assert!(app.check_reentry_loop(&ctx, false).is_none());
    let node = app
        .check_reentry_loop(&ctx, false)
        .expect("entry beyond the threshold must trigger the guard");
    match node.action {
        EntryAction::Transfer { target, .. } => assert_eq!(target, "ivr:default_ivr"),
        other => panic!("expected direct IVR fallback transfer, got {other:?}"),
    }

    // One-shot: the NEXT restart's guard sees the fallback marker (shared
    // via session vars) and hangs up instead of transferring again.
    let mut restart = StepIvrApp::with_provider(Box::new(MockProvider::new(vec![])))
        .with_name("main")
        .with_max_flow_restarts(1)
        .with_ivr_fallback(Some(mk_fb()));
    restart.runtime_vars = Some(ctx.session_vars.clone());
    let node = restart
        .check_reentry_loop(&ctx, false)
        .expect("guard must trigger again after the restart");
    match node.action {
        EntryAction::Prompt { file: Some(f), .. } => assert_eq!(f, "sounds/error.wav"),
        other => {
            panic!("expected error-tone hangup chain after one-shot fallback, got {other:?}")
        }
    }
}

#[tokio::test]
async fn test_step_ivr_reentry_state_not_forwarded_to_provider() {
    let ctx = make_test_context();

    // Cycle 1 writes the guard state into shared session vars.
    {
        let provider = Arc::new(MockProvider::new(vec![ActionNode::new(EntryAction::Exit)]));
        let app = StepIvrApp::with_provider(Box::new(MockProviderHandle(provider.clone())))
            .with_name("loop-ivr")
            .with_max_flow_restarts(5);
        let mut stack = MockCallStack::run_with_context(Box::new(app), ctx.clone());
        stack
            .assert_cmd(2000, "answer", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack.join().await.unwrap();
    }
    assert!(
        ctx.session_vars.contains_key(IVR_REENTRY_STATE_KEY),
        "guard state must persist in shared session vars"
    );

    // Cycle 2 must NOT leak the guard state into ProviderContext.variables.
    let provider = Arc::new(MockProvider::new(vec![ActionNode::new(EntryAction::Exit)]));
    let app = StepIvrApp::with_provider(Box::new(MockProviderHandle(provider.clone())))
        .with_name("loop-ivr")
        .with_max_flow_restarts(5);
    let mut stack = MockCallStack::run_with_context(Box::new(app), ctx.clone());
    stack
        .assert_cmd(2000, "answer", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack.join().await.unwrap();

    let contexts = provider.contexts.lock().unwrap();
    assert!(!contexts.is_empty(), "cycle 2 must have called /step");
    for ctx in contexts.iter() {
        assert!(
            !ctx.variables.contains_key(IVR_REENTRY_STATE_KEY),
            "guard bookkeeping must never reach the provider payload"
        );
    }
}

/// The reported bug: a provider flow whose last node never terminates the
/// call (`exit` here — same class as queue/transfer back into this IVR)
/// makes the session restart the IVR forever. Each restart is a fresh
/// start→step→end cycle. The guard must break the cycle on the entry
/// after the threshold WITHOUT reaching the provider.
#[tokio::test]
async fn test_step_ivr_reentry_loop_hangs_up_without_provider_start() {
    let ctx = make_test_context();

    // Cycles 1-3: each restart runs a full flow (start → step → exit/end).
    for cycle in 1..=3 {
        let provider = Arc::new(MockProvider::new(vec![ActionNode::new(EntryAction::Exit)]));
        let app = StepIvrApp::with_provider(Box::new(MockProviderHandle(provider.clone())))
            .with_name("loop-ivr")
            .with_max_flow_restarts(3);
        let mut stack = MockCallStack::run_with_context(Box::new(app), ctx.clone());
        stack
            .assert_cmd(2000, "answer", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack.join().await.expect("cycle must complete cleanly");
        assert!(
            *provider.start_called.lock().unwrap(),
            "cycle {cycle} must reach /start"
        );
    }

    // Cycle 4: the guard fires BEFORE /start — error tone + hangup.
    let provider = Arc::new(MockProvider::new(vec![]));
    let app = StepIvrApp::with_provider(Box::new(MockProviderHandle(provider.clone())))
        .with_name("loop-ivr")
        .with_max_flow_restarts(3);
    let mut stack = MockCallStack::run_with_context(Box::new(app), ctx.clone());
    stack
        .assert_cmd(2000, "answer", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .assert_cmd(2000, "error-tone", |c| play_file_cmd(c, "sounds/error.wav"))
        .await;
    stack.audio_complete("ivr_prompt");
    stack
        .assert_cmd(2000, "hangup", |c| matches!(c, CallCommand::Hangup { .. }))
        .await;
    stack.join().await.unwrap();
    assert!(
        !*provider.start_called.lock().unwrap(),
        "guard must block the provider /start on the looping entry"
    );
    assert!(
        provider.contexts.lock().unwrap().is_empty(),
        "guard must also block the first /step"
    );
}

/// Regression for the c32571e0 guard misfiring on the per-node bridge
/// progression: each node's resumable bridge hand-off returns to the app
/// and re-enters the IVR with `ivr_resumed=1`, so the NEXT node is served
/// on a fresh /start. Every resumed entry serves a DIFFERENT node — flow
/// progress, never a restart — so with `max_flow_restarts=3` the guard
/// must stay silent across all 6 nodes (entry → prompt_tts →
/// prompt_ttsbreak → prompt_tts_api → menu_tts → syshangup) instead of
/// silently killing the call at node 4.
#[tokio::test]
async fn test_step_ivr_reentry_guard_allows_resumed_node_progression() {
    let ctx = make_test_context();
    let nodes = [
        "entry",
        "prompt_tts",
        "prompt_ttsbreak",
        "prompt_tts_api",
        "menu_tts",
        "syshangup",
    ];
    for (i, step_id) in nodes.iter().enumerate() {
        let mut node = ActionNode::new(EntryAction::Exit);
        node.step_id = Some((*step_id).to_string());
        let provider = Arc::new(MockProvider::new(vec![node]));
        let mut app = StepIvrApp::with_provider(Box::new(MockProviderHandle(provider.clone())))
            .with_name("main")
            .with_max_flow_restarts(3);
        if i > 0 {
            // Bridge disconnect → return-app re-entry marks the flow as
            // resumed (`ivr_resumed=1`), the per-node progression path.
            app = app.with_ivr_params(serde_json::json!({ "ivr_resumed": "1" }));
        }
        let mut stack = MockCallStack::run_with_context(Box::new(app), ctx.clone());
        stack
            .assert_cmd(2000, "answer", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack.join().await.expect("cycle must complete cleanly");
        assert!(
            *provider.start_called.lock().unwrap(),
            "node {step_id} (cycle {}) must reach /start — node progress must never trip the guard",
            i + 1
        );
    }
}

/// The guard's original target stays protected under resumed entries: a
/// provider serving the SAME node again on every bridge/queue return
/// (stateless flow, no DTMF) is a real loop. Cycles 2-3 pass (within the
/// threshold); on cycle 4 the repeated-node count passes 3 and the call
/// is diverted — the node itself never executes.
#[tokio::test]
async fn test_step_ivr_reentry_guard_blocks_resumed_same_node_loop() {
    let ctx = make_test_context();

    // Cycle 1 (fresh entry): the flow serves node "q", which hands the
    // call to a queue that later returns to this IVR.
    {
        let mut node = ActionNode::new(EntryAction::Exit);
        node.step_id = Some("q".into());
        let provider = Arc::new(MockProvider::new(vec![node]));
        let app = StepIvrApp::with_provider(Box::new(MockProviderHandle(provider.clone())))
            .with_name("main")
            .with_max_flow_restarts(3);
        let mut stack = MockCallStack::run_with_context(Box::new(app), ctx.clone());
        stack
            .assert_cmd(2000, "answer", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack.join().await.unwrap();
    }

    // Cycles 2-4 (resumed entries): the provider serves "q" again.
    for cycle in 2..=4 {
        let mut node = ActionNode::new(EntryAction::Exit);
        node.step_id = Some("q".into());
        let provider = Arc::new(MockProvider::new(vec![node]));
        let app = StepIvrApp::with_provider(Box::new(MockProviderHandle(provider.clone())))
            .with_name("main")
            .with_max_flow_restarts(3)
            .with_ivr_params(serde_json::json!({ "ivr_resumed": "1" }));
        let mut stack = MockCallStack::run_with_context(Box::new(app), ctx.clone());
        stack
            .assert_cmd(2000, "answer", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        if cycle < 4 {
            stack
                .join()
                .await
                .expect("repeated-node cycle within the threshold must complete");
        } else {
            // Repeated-node count passes the threshold → divert: error
            // tone + hangup, the looping node is never executed.
            stack
                .assert_cmd(2000, "error-tone", |c| play_file_cmd(c, "sounds/error.wav"))
                .await;
            stack.audio_complete("ivr_prompt");
            stack
                .assert_cmd(2000, "hangup", |c| matches!(c, CallCommand::Hangup { .. }))
                .await;
            stack.join().await.unwrap();
            assert!(
                *provider.start_called.lock().unwrap(),
                "the post-`/step` phase diverts after the provider round-trip"
            );
        }
    }
}

/// Buffered bridge DTMF (resume path) is real caller input: it must clear
/// the guard in `on_enter` BEFORE the pre-`/start` check, or a
/// loop-tainted counter would divert a legitimate resume with digits.
#[tokio::test]
async fn test_step_ivr_reentry_guard_cleared_by_buffered_bridge_digits() {
    let ctx = make_test_context();
    // Taint the counter past the threshold.
    let tainted = serde_json::json!({
        "main": { "count": 9, "ts": chrono::Utc::now().to_rfc3339() }
    });
    ctx.session_vars
        .insert(IVR_REENTRY_STATE_KEY.to_string(), tainted.to_string());

    let provider = Arc::new(MockProvider::new(vec![ActionNode::new(EntryAction::Exit)]));
    let app = StepIvrApp::with_provider(Box::new(MockProviderHandle(provider.clone())))
        .with_name("main")
        .with_max_flow_restarts(3)
        .with_ivr_params(serde_json::json!({
            "ivr_resumed": "1",
            "bridge_dtmf_digits": "5",
        }));
    let mut stack = MockCallStack::run_with_context(Box::new(app), ctx.clone());
    stack
        .assert_cmd(2000, "answer", |c| matches!(c, CallCommand::Answer { .. }))
        .await;
    stack
        .join()
        .await
        .expect("real caller input must clear the guard — the resume must run");
    assert!(
        *provider.start_called.lock().unwrap(),
        "must reach /start — buffered digits prove progress"
    );
}
