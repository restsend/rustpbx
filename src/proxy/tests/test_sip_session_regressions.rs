use super::common::{
    create_test_request, create_test_server, create_test_server_with_config, create_transaction,
};
use crate::call::domain::{LegId, MediaCapability, MediaPathMode};
use crate::call::runtime::{AppDescriptor, AppRuntime, AppRuntimeError, AppStatus};
use crate::call::{
    DialDirection, DialStrategy, Dialplan, FailureAction, MediaConfig, QueueFallbackAction,
    QueuePlan, TransactionCookie,
};
use crate::config::{MediaProxyMode, ProxyConfig};
use crate::proxy::proxy_call::sip_session::SipSession;
use crate::proxy::proxy_call::state::CallContext;
use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;
use crate::proxy::routing::{
    RouteQueueConfig, RouteQueueFallbackConfig, RouteQueueStrategyConfig, RouteQueueTargetConfig,
};
use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct AlreadyRunningThenOkRuntime {
    start_calls: AtomicUsize,
    stop_calls: AtomicUsize,
    stop_returns_not_running: bool,
    second_start_should_fail: bool,
}

impl AlreadyRunningThenOkRuntime {
    fn new() -> Self {
        Self {
            start_calls: AtomicUsize::new(0),
            stop_calls: AtomicUsize::new(0),
            stop_returns_not_running: false,
            second_start_should_fail: false,
        }
    }

    fn with_stop_not_running(mut self) -> Self {
        self.stop_returns_not_running = true;
        self
    }

    fn with_second_start_error(mut self) -> Self {
        self.second_start_should_fail = true;
        self
    }
}

#[async_trait]
impl AppRuntime for AlreadyRunningThenOkRuntime {
    async fn start_app(
        &self,
        app_name: &str,
        _params: Option<serde_json::Value>,
        _auto_answer: bool,
    ) -> crate::call::runtime::AppResult<()> {
        let idx = self.start_calls.fetch_add(1, Ordering::SeqCst);
        if idx == 0 {
            return Err(AppRuntimeError::AlreadyRunning(app_name.to_string()));
        }
        if self.second_start_should_fail {
            return Err(AppRuntimeError::UnknownApp(app_name.to_string()));
        }
        Ok(())
    }

    async fn stop_app(&self, _reason: Option<String>) -> crate::call::runtime::AppResult<()> {
        self.stop_calls.fetch_add(1, Ordering::SeqCst);
        if self.stop_returns_not_running {
            return Err(AppRuntimeError::NotRunning);
        }
        Ok(())
    }

    fn inject_event(&self, _event: serde_json::Value) -> crate::call::runtime::AppResult<()> {
        Ok(())
    }

    fn is_running(&self) -> bool {
        false
    }

    fn status(&self) -> AppStatus {
        AppStatus::Idle
    }

    fn current_app(&self) -> Option<String> {
        None
    }

    fn required_capabilities(&self) -> Vec<MediaCapability> {
        vec![]
    }

    fn app_descriptor(&self, _app_name: &str) -> Option<AppDescriptor> {
        None
    }
}

struct AlwaysFailStartRuntime;

struct StartOnlyRuntime {
    start_calls: AtomicUsize,
}

impl StartOnlyRuntime {
    fn new() -> Self {
        Self {
            start_calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl AppRuntime for AlwaysFailStartRuntime {
    async fn start_app(
        &self,
        app_name: &str,
        _params: Option<serde_json::Value>,
        _auto_answer: bool,
    ) -> crate::call::runtime::AppResult<()> {
        Err(AppRuntimeError::UnknownApp(app_name.to_string()))
    }

    async fn stop_app(&self, _reason: Option<String>) -> crate::call::runtime::AppResult<()> {
        Ok(())
    }

    fn inject_event(&self, _event: serde_json::Value) -> crate::call::runtime::AppResult<()> {
        Ok(())
    }

    fn is_running(&self) -> bool {
        false
    }

    fn status(&self) -> AppStatus {
        AppStatus::Idle
    }

    fn current_app(&self) -> Option<String> {
        None
    }

    fn required_capabilities(&self) -> Vec<MediaCapability> {
        vec![]
    }

    fn app_descriptor(&self, _app_name: &str) -> Option<AppDescriptor> {
        None
    }
}

#[async_trait]
impl AppRuntime for StartOnlyRuntime {
    async fn start_app(
        &self,
        _app_name: &str,
        _params: Option<serde_json::Value>,
        _auto_answer: bool,
    ) -> crate::call::runtime::AppResult<()> {
        self.start_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn stop_app(&self, _reason: Option<String>) -> crate::call::runtime::AppResult<()> {
        Ok(())
    }

    fn inject_event(&self, _event: serde_json::Value) -> crate::call::runtime::AppResult<()> {
        Ok(())
    }

    fn is_running(&self) -> bool {
        false
    }

    fn status(&self) -> AppStatus {
        AppStatus::Idle
    }

    fn current_app(&self) -> Option<String> {
        None
    }

    fn required_capabilities(&self) -> Vec<MediaCapability> {
        vec![]
    }

    fn app_descriptor(&self, _app_name: &str) -> Option<AppDescriptor> {
        None
    }
}

async fn build_session(dialplan: Dialplan) -> SipSession {
    let (server, _) = create_test_server().await;
    build_session_on_server(server, dialplan).await
}

async fn build_session_with_config(dialplan: Dialplan, config: ProxyConfig) -> SipSession {
    let (server, _) = create_test_server_with_config(config).await;
    build_session_on_server(server, dialplan).await
}

async fn build_session_on_server(
    server: Arc<crate::proxy::server::SipServerInner>,
    dialplan: Dialplan,
) -> SipSession {
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let (tx, _) = create_transaction(request).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let context = CallContext {
        session_id: "test-session".to_string(),
        dialplan: Arc::new(dialplan),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:ivr@rustpbx.com".to_string(),
        max_forwards: 70,
        dtmf_digits: Vec::new(),
    };

    let caller_peer = Arc::new(MockMediaPeer::new());
    let callee_peer = Arc::new(MockMediaPeer::new());
    let use_media_proxy =
        SipSession::check_media_proxy(&context, &context.dialplan.media.proxy_mode);
    let (session, _handle, _cmd_rx) = SipSession::new(
        server,
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        use_media_proxy,
        caller_peer,
        callee_peer,
    );
    session
}

fn build_dialplan_with_mode(mode: MediaProxyMode) -> Dialplan {
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    Dialplan::new("test-session".to_string(), request, DialDirection::Inbound)
        .with_media(MediaConfig::new().with_proxy_mode(mode))
}

fn make_queue_hangup_config(queue_name: &str) -> ProxyConfig {
    let mut config = ProxyConfig::default();
    config.queues.insert(
        queue_name.to_string(),
        RouteQueueConfig {
            name: Some(queue_name.to_string()),
            strategy: RouteQueueStrategyConfig {
                targets: vec![RouteQueueTargetConfig {
                    uri: "skill-group:missing".to_string(),
                    label: Some("missing-skill-group".to_string()),
                }],
                ..Default::default()
            },
            fallback: Some(RouteQueueFallbackConfig {
                failure_code: Some(486),
                failure_reason: Some("All agents unavailable".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    config
}

#[tokio::test]
async fn test_media_proxy_auto_anchors_application_flow() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_application(
        "ivr".to_string(),
        None,
        true,
    );

    let session = build_session(dialplan).await;
    assert_eq!(session.media_profile.path, MediaPathMode::Anchored);
}

#[tokio::test]
async fn test_media_proxy_auto_anchors_queue_flow() {
    let queue_plan = QueuePlan {
        queue_name: "support".to_string(),
        ..Default::default()
    };
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_queue(queue_plan);

    let session = build_session(dialplan).await;
    assert_eq!(session.media_profile.path, MediaPathMode::Anchored);
}

#[tokio::test]
async fn test_media_proxy_auto_keeps_plain_targets_bypass_without_recording() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto)
        .with_targets(DialStrategy::Sequential(vec![]));

    let session = build_session(dialplan).await;
    assert_eq!(session.media_profile.path, MediaPathMode::Bypass);
}

#[tokio::test]
async fn test_start_ivr_app_restarts_after_already_running() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_application(
        "ivr".to_string(),
        None,
        true,
    );
    let mut session = build_session(dialplan).await;

    let runtime = Arc::new(AlreadyRunningThenOkRuntime::new());
    session.app_runtime = runtime.clone();

    session
        .start_ivr_app("hello")
        .await
        .expect("start_ivr_app should recover from AlreadyRunning");

    assert_eq!(runtime.start_calls.load(Ordering::SeqCst), 2);
    assert_eq!(runtime.stop_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_start_ivr_app_restarts_even_if_stop_reports_not_running() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_application(
        "ivr".to_string(),
        None,
        true,
    );
    let mut session = build_session(dialplan).await;

    let runtime = Arc::new(AlreadyRunningThenOkRuntime::new().with_stop_not_running());
    session.app_runtime = runtime.clone();

    session
        .start_ivr_app("hello")
        .await
        .expect("restart should continue when stop_app returns NotRunning");

    assert_eq!(runtime.start_calls.load(Ordering::SeqCst), 2);
    assert_eq!(runtime.stop_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_start_ivr_app_propagates_non_retryable_start_error() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_application(
        "ivr".to_string(),
        None,
        true,
    );
    let mut session = build_session(dialplan).await;

    session.app_runtime = Arc::new(AlwaysFailStartRuntime);

    let err = session
        .start_ivr_app("hello")
        .await
        .expect_err("non-AlreadyRunning error should be returned");
    assert!(
        err.to_string().contains("Failed to start IVR 'hello'"),
        "unexpected error: {}",
        err
    );
}

#[tokio::test]
async fn test_start_ivr_app_reports_restart_failure_when_second_start_fails() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_application(
        "ivr".to_string(),
        None,
        true,
    );
    let mut session = build_session(dialplan).await;

    let runtime = Arc::new(AlreadyRunningThenOkRuntime::new().with_second_start_error());
    session.app_runtime = runtime.clone();

    let err = session
        .start_ivr_app("hello")
        .await
        .expect_err("second start failure should be surfaced");
    assert!(
        err.to_string().contains("Failed to restart IVR 'hello'"),
        "unexpected error: {}",
        err
    );
    assert_eq!(runtime.start_calls.load(Ordering::SeqCst), 2);
    assert_eq!(runtime.stop_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn test_queue_fallback_without_prompt_maps_to_hangup() {
    let queue = RouteQueueConfig {
        name: Some("support".to_string()),
        fallback: Some(RouteQueueFallbackConfig {
            failure_code: Some(486),
            failure_reason: Some("All agents unavailable".to_string()),
            failure_prompt: None,
            ..Default::default()
        }),
        ..Default::default()
    };

    let plan = queue.to_queue_plan().expect("queue plan should build");
    match plan.fallback {
        Some(QueueFallbackAction::Failure(FailureAction::Hangup { .. })) => {}
        other => panic!("expected Hangup fallback, got {:?}", other),
    }
}

#[test]
fn test_queue_fallback_with_prompt_maps_to_play_then_hangup() {
    let queue = RouteQueueConfig {
        name: Some("support".to_string()),
        fallback: Some(RouteQueueFallbackConfig {
            failure_code: Some(486),
            failure_reason: Some("All agents unavailable".to_string()),
            failure_prompt: Some("sounds/queue-fallback.wav".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };

    let plan = queue.to_queue_plan().expect("queue plan should build");
    match plan.fallback {
        Some(QueueFallbackAction::Failure(FailureAction::PlayThenHangup {
            audio_file,
            ..
        })) => assert_eq!(audio_file, "sounds/queue-fallback.wav"),
        other => panic!("expected PlayThenHangup fallback, got {:?}", other),
    }
}

#[tokio::test]
async fn test_queue_transfer_without_return_ivr_keeps_hangup_fallback_when_no_agents() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_application(
        "ivr".to_string(),
        None,
        true,
    );
    let config = make_queue_hangup_config("support");
    let mut session = build_session_with_config(dialplan, config).await;

    let err = session
        .handle_queue_transfer(LegId::from("caller"), "support", None)
        .await
        .expect_err("without return_ivr, hangup fallback should surface failure");
    assert!(
        err.to_string().contains("Queue transfer failed"),
        "unexpected error: {}",
        err
    );
}

#[tokio::test]
async fn test_queue_transfer_return_ivr_overrides_hangup_fallback_when_no_agents() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_application(
        "ivr".to_string(),
        None,
        true,
    );
    let config = make_queue_hangup_config("support");
    let mut session = build_session_with_config(dialplan, config).await;

    let runtime = Arc::new(StartOnlyRuntime::new());
    session.app_runtime = runtime.clone();

    session
        .handle_queue_transfer(
            LegId::from("caller"),
            "support",
            Some("hello".to_string()),
        )
        .await
        .expect("return_ivr should override hangup fallback and start IVR");

    assert_eq!(runtime.start_calls.load(Ordering::SeqCst), 1);
}
