use super::common::{
    create_test_request, create_test_server, create_test_server_with_config,
    create_test_server_with_config_and_sipflow_backend, create_transaction,
};
use crate::call::app::{AppInvocationContext, ApplicationContext, CallInfo};
use crate::call::domain::{CallCommand, Leg, LegId, LegState, MediaPathMode, ReturnAppSpec};
use crate::call::runtime::{AppRuntime, AppRuntimeError, BridgeConfig};
use crate::call::{
    DialDirection, DialStrategy, Dialplan, FailureAction, MediaConfig, QueueFallbackAction,
    QueuePlan, TransactionCookie,
};
use crate::config::{MediaProxyMode, ProxyConfig};
use crate::proxy::proxy_call::session_hooks::CallSessionContext;
use crate::proxy::proxy_call::sip_session::SipSession;
use crate::proxy::proxy_call::state::CallContext;
use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;
use crate::proxy::routing::{
    RouteQueueConfig, RouteQueueFallbackConfig, RouteQueueStrategyConfig, RouteQueueTargetConfig,
};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct CountingSipflowBackend {
    rtp_records: AtomicUsize,
}

#[async_trait]
impl crate::sipflow::SipFlowBackend for CountingSipflowBackend {
    fn record(
        &self,
        _call_id: std::borrow::Cow<'_, str>,
        item: crate::sipflow::SipFlowItem,
    ) -> anyhow::Result<()> {
        if item.msg_type == crate::sipflow::SipFlowMsgType::Rtp {
            self.rtp_records.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn query_flow(
        &self,
        _call_id: &str,
        _start_time: chrono::DateTime<chrono::Local>,
        _end_time: chrono::DateTime<chrono::Local>,
    ) -> anyhow::Result<Vec<crate::sipflow::SipFlowItem>> {
        Ok(Vec::new())
    }

    async fn query_media_stats(
        &self,
        _call_id: &str,
        _start_time: chrono::DateTime<chrono::Local>,
        _end_time: chrono::DateTime<chrono::Local>,
    ) -> anyhow::Result<Vec<crate::sipflow::SipFlowMediaStats>> {
        Ok(Vec::new())
    }

    async fn query_media(
        &self,
        _call_id: &str,
        _start_time: chrono::DateTime<chrono::Local>,
        _end_time: chrono::DateTime<chrono::Local>,
    ) -> anyhow::Result<Vec<u8>> {
        Ok(Vec::new())
    }
}

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

    fn current_app(&self) -> Option<String> {
        None
    }
}

struct AlwaysFailStartRuntime;

struct StartOnlyRuntime {
    start_calls: AtomicUsize,
}

struct BridgeReturnRuntime {
    running: AtomicBool,
    inject_calls: AtomicUsize,
    started_params: std::sync::Mutex<Vec<Option<serde_json::Value>>>,
}

impl BridgeReturnRuntime {
    fn new() -> Self {
        Self {
            running: AtomicBool::new(true),
            inject_calls: AtomicUsize::new(0),
            started_params: std::sync::Mutex::new(Vec::new()),
        }
    }
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

    fn current_app(&self) -> Option<String> {
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

    fn current_app(&self) -> Option<String> {
        None
    }
}

#[async_trait]
impl AppRuntime for BridgeReturnRuntime {
    async fn start_app(
        &self,
        app_name: &str,
        params: Option<serde_json::Value>,
        _auto_answer: bool,
    ) -> crate::call::runtime::AppResult<()> {
        if self.running.swap(true, Ordering::SeqCst) {
            return Err(AppRuntimeError::AlreadyRunning(app_name.to_string()));
        }
        self.started_params.lock().unwrap().push(params);
        Ok(())
    }

    async fn stop_app(&self, _reason: Option<String>) -> crate::call::runtime::AppResult<()> {
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn inject_event(&self, _event: serde_json::Value) -> crate::call::runtime::AppResult<()> {
        self.inject_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    fn current_app(&self) -> Option<String> {
        self.is_running().then(|| "ivr".to_string())
    }
}

/// Test runtime that records the name of every app started, so tests can
/// assert which app (e.g. "ivr" vs "queue") a fallback path launched.
struct NameCapturingRuntime {
    started_apps: std::sync::Mutex<Vec<String>>,
}

struct RoutePointRuntime {
    started_apps: std::sync::Mutex<Vec<(String, Option<serde_json::Value>)>>,
    failed_apps: Vec<String>,
    invocation: Option<AppInvocationContext>,
    context: Option<Arc<ApplicationContext>>,
}

impl RoutePointRuntime {
    fn new(failed_apps: &[&str]) -> Self {
        Self {
            started_apps: std::sync::Mutex::new(Vec::new()),
            failed_apps: failed_apps.iter().map(|name| name.to_string()).collect(),
            invocation: None,
            context: None,
        }
    }

    fn started_apps(&self) -> Vec<(String, Option<serde_json::Value>)> {
        self.started_apps.lock().unwrap().clone()
    }
}

#[async_trait]
impl AppRuntime for RoutePointRuntime {
    fn app_context(&self) -> Option<&Arc<ApplicationContext>> {
        self.context.as_ref()
    }

    async fn start_app(
        &self,
        app_name: &str,
        params: Option<serde_json::Value>,
        _auto_answer: bool,
    ) -> crate::call::runtime::AppResult<()> {
        self.started_apps
            .lock()
            .unwrap()
            .push((app_name.to_string(), params));
        if self.failed_apps.iter().any(|name| name == app_name) {
            return Err(AppRuntimeError::UnknownApp(app_name.to_string()));
        }
        Ok(())
    }

    async fn current_app_invocation(&self) -> Option<AppInvocationContext> {
        self.invocation.clone()
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

    fn current_app(&self) -> Option<String> {
        None
    }
}

impl NameCapturingRuntime {
    fn new() -> Self {
        Self {
            started_apps: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn started_apps(&self) -> Vec<String> {
        self.started_apps.lock().unwrap().clone()
    }
}

#[async_trait]
impl AppRuntime for NameCapturingRuntime {
    async fn start_app(
        &self,
        app_name: &str,
        _params: Option<serde_json::Value>,
        _auto_answer: bool,
    ) -> crate::call::runtime::AppResult<()> {
        self.started_apps.lock().unwrap().push(app_name.to_string());
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

    fn current_app(&self) -> Option<String> {
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
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
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

fn recording_test_offer() -> String {
    "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 40000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n".to_string()
}

async fn setup_recording_test_media(session: &mut SipSession) {
    session.media.caller_offer = Some(recording_test_offer());
    session
        .create_callee_track(false)
        .await
        .expect("create recording test media");
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
async fn test_connected_dynamic_leg_failure_hangs_up_caller() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_queue(QueuePlan {
        queue_name: "support".to_string(),
        ..Default::default()
    });
    let mut session = build_session(dialplan).await;
    let caller_leg = LegId::from("caller");
    let agent_leg = LegId::from("queue-agent");
    let mut leg = Leg::new(agent_leg.clone());
    leg.state = LegState::Connected;
    session.legs.insert(agent_leg.clone(), leg);
    session.bridge = BridgeConfig::bridge(caller_leg, agent_leg.clone());

    let caller_dialog_id = session
        .caller_dialog
        .as_ref()
        .map(|d| d.id())
        .expect("caller dialog present");
    session
        .execute_command(
            CallCommand::LegFailed {
                leg_id: agent_leg,
                reason: "Remote hung up".to_string(),
            },
            None,
        )
        .await;

    assert!(session.pending_hangup.contains(&caller_dialog_id));
}

#[tokio::test]
async fn test_connected_dynamic_leg_failure_returns_to_ivr_when_set() {
    // Regression: when meta.transfer_return_to_ivr is set and a connected
    // dynamic leg (queue agent) hangs up, the caller should be returned to
    // the IVR app instead of being hung up.
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_queue(QueuePlan {
        queue_name: "support".to_string(),
        ..Default::default()
    });
    let mut session = build_session(dialplan).await;
    let caller_leg = LegId::from("caller");
    let agent_leg = LegId::from("queue-agent");
    let mut leg = Leg::new(agent_leg.clone());
    leg.state = LegState::Connected;
    session.legs.insert(agent_leg.clone(), leg);
    session.bridge = BridgeConfig::bridge(caller_leg, agent_leg.clone());

    // Simulate a queue-transfer that set return_app
    session.meta.transfer_return_app = Some(ReturnAppSpec {
        app_name: "ivr".to_string(),
        params: serde_json::json!({"file": "main-menu"}),
    });

    let runtime = Arc::new(StartOnlyRuntime::new());
    session.app_runtime = runtime.clone();

    session
        .execute_command(
            CallCommand::LegFailed {
                leg_id: agent_leg,
                reason: "Remote hung up".to_string(),
            },
            None,
        )
        .await;

    // start_ivr_app -> ensure_app_running -> start_app("ivr") should have fired.
    assert_eq!(
        runtime.start_calls.load(Ordering::SeqCst),
        1,
        "IVR app should be started on agent hangup"
    );
    // transfer_return_to_ivr should be consumed
    assert!(session.meta.transfer_return_app.is_none());
    // Caller should NOT be in pending_hangup (IVR took over)
    let caller_dialog_id = session
        .caller_dialog
        .as_ref()
        .map(|d| d.id())
        .expect("caller dialog present");
    assert!(!session.pending_hangup.contains(&caller_dialog_id));
}

#[tokio::test]
async fn bridge_rtp_dtmf_reaches_return_app_once_without_stale_app_injection() {
    use crate::media::leg::{LegConfig, LegInner};
    use crate::media::media_bridge::LegSide;
    use rustrtc::peer_connection::RtpObserver;

    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_application(
        "ivr".to_string(),
        None,
        true,
    );
    let mut session = build_session(dialplan).await;
    let runtime = Arc::new(BridgeReturnRuntime::new());
    session.app_runtime = runtime.clone();

    let (bridge_tx, mut bridge_rx) = mpsc::unbounded_channel();
    *session.bridge_dtmf_tx.write() = Some(bridge_tx);
    session.meta.transfer_return_app = Some(ReturnAppSpec {
        app_name: "ivr".to_string(),
        params: serde_json::json!({"file": "main-menu"}),
    });

    let caller_leg = LegInner::new("caller", &LegConfig::rtp_pcmu(), None).unwrap();
    caller_leg.ingress_tap().set_dtmf_payload_types(vec![101]);
    session
        .media
        .bridge
        .as_mut()
        .expect("anchored media bridge")
        .replace_leg(LegSide::A, caller_leg.clone())
        .await;

    let packet = rustrtc::rtp::RtpPacket::new(
        rustrtc::rtp::RtpHeader::new(101, 1, 160, 1234),
        vec![6, 0x80, 0, 160],
    );
    caller_leg
        .ingress_tap()
        .on_ingress(&packet, "127.0.0.1:40000".parse().unwrap());

    let bridge_event = tokio::time::timeout(std::time::Duration::from_secs(1), bridge_rx.recv())
        .await
        .expect("bridge DTMF timeout")
        .expect("bridge DTMF channel closed");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&bridge_event).unwrap()["digit"],
        "6"
    );
    assert_eq!(runtime.inject_calls.load(Ordering::SeqCst), 0);

    // Bridge cleanup disarms forwarding before the stored return app resumes.
    *session.bridge_dtmf_tx.write() = None;
    session
        .execute_command(CallCommand::StartReturnApp, None)
        .await;

    let started_params = runtime.started_params.lock().unwrap();
    assert_eq!(started_params.len(), 1);
    assert_eq!(
        started_params[0].as_ref().unwrap()["ivr_params"]["bridge_dtmf_digits"],
        "6"
    );
}

#[tokio::test]
async fn test_media_proxy_auto_keeps_plain_targets_bypass_without_recording() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto)
        .with_targets(DialStrategy::Sequential(vec![]));

    let session = build_session(dialplan).await;
    assert_eq!(session.media_profile.path, MediaPathMode::Bypass);
}

#[tokio::test]
async fn recording_disabled_does_not_create_capture_task() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_application(
        "ivr".to_string(),
        None,
        true,
    );
    let mut session = build_session(dialplan).await;

    setup_recording_test_media(&mut session).await;

    assert!(
        !session
            .media
            .bridge
            .as_ref()
            .expect("media bridge")
            .has_recorder_task(),
        "disabled recording must not create a capture sender/task"
    );
    if let Some(mut bridge) = session.media.bridge.take() {
        bridge.close();
    }
}

#[tokio::test]
async fn recording_enabled_manual_start_creates_idle_capture_task() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("manual.wav").to_string_lossy().into_owned();
    let recording = crate::call::CallRecordingConfig {
        enabled: true,
        option: Some(crate::media::recorder::RecorderOption::new(path.clone())),
        auto_start: false,
        auto_start_at: crate::config::RecordingAutoStartAt::Media,
        recording_type: crate::config::RecordingType::Local,
        stereo_swap: false,
    };
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto)
        .with_application("ivr".to_string(), None, true)
        .with_recording(recording);
    let mut session = build_session(dialplan).await;

    setup_recording_test_media(&mut session).await;

    assert!(
        session
            .media
            .bridge
            .as_ref()
            .expect("media bridge")
            .has_recorder_task(),
        "enabled recording must prepare the capture sender/task"
    );
    assert!(
        !std::path::Path::new(&path).exists(),
        "auto_start=false must leave the recorder backend idle"
    );
    if let Some(mut bridge) = session.media.bridge.take() {
        bridge.stop_recording().await.expect("stop idle recorder");
        bridge.close();
    }
}

#[tokio::test]
async fn file_recording_default_starts_at_media_setup() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("auto.wav").to_string_lossy().into_owned();
    let recording = crate::call::CallRecordingConfig {
        enabled: true,
        option: Some(crate::media::recorder::RecorderOption::new(path.clone())),
        auto_start: true,
        auto_start_at: crate::config::RecordingAutoStartAt::Media,
        recording_type: crate::config::RecordingType::Local,
        stereo_swap: false,
    };
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto)
        .with_application("ivr".to_string(), None, true)
        .with_recording(recording);
    let mut session = build_session(dialplan).await;

    setup_recording_test_media(&mut session).await;
    assert!(
        session
            .media
            .bridge
            .as_ref()
            .expect("media bridge")
            .has_recorder()
            .await,
        "caller media setup must install the recorder implementation"
    );

    assert!(
        std::path::Path::new(&path).exists(),
        "media timing must initialize the file recorder during caller media setup"
    );
    if let Some(mut bridge) = session.media.bridge.take() {
        assert!(
            bridge
                .stop_recording()
                .await
                .expect("stop file recorder")
                .is_some(),
            "active file recorder must return a finalized result"
        );
        bridge.close();
    }
}

#[tokio::test]
async fn file_recording_answer_timing_waits_after_media_setup() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir
        .path()
        .join("answer-only.wav")
        .to_string_lossy()
        .into_owned();
    let recording = crate::call::CallRecordingConfig {
        enabled: true,
        option: Some(crate::media::recorder::RecorderOption::new(path.clone())),
        auto_start: true,
        auto_start_at: crate::config::RecordingAutoStartAt::Answer,
        recording_type: crate::config::RecordingType::Local,
        stereo_swap: false,
    };
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto)
        .with_application("ivr".to_string(), None, true)
        .with_recording(recording);
    let mut session = build_session(dialplan).await;

    setup_recording_test_media(&mut session).await;
    assert!(
        !std::path::Path::new(&path).exists(),
        "answer timing must not install the recorder during caller media setup"
    );
    assert!(
        !session
            .media
            .bridge
            .as_ref()
            .expect("media bridge")
            .has_recorder()
            .await,
        "the recording task must remain implementation-free until answer"
    );

    session
        .set_auto_recorder()
        .await
        .expect("start recorder at final answer");
    assert!(
        session
            .media
            .bridge
            .as_ref()
            .expect("media bridge")
            .has_recorder()
            .await,
        "the recording task must report the final-answer implementation"
    );
    assert!(
        std::path::Path::new(&path).exists(),
        "answer timing must install the recorder at the final answer"
    );
    if let Some(mut bridge) = session.media.bridge.take() {
        bridge.stop_recording().await.expect("stop file recorder");
        bridge.close();
    }
}

#[tokio::test]
async fn sipflow_recording_default_starts_at_media_setup() {
    use rustrtc::peer_connection::RtpObserver;

    let backend = Arc::new(CountingSipflowBackend {
        rtp_records: AtomicUsize::new(0),
    });
    let (server, _) = create_test_server_with_config_and_sipflow_backend(
        ProxyConfig::default(),
        Some(backend.clone()),
    )
    .await;
    let recording = crate::call::CallRecordingConfig {
        enabled: true,
        option: None,
        auto_start: true,
        auto_start_at: crate::config::RecordingAutoStartAt::Media,
        recording_type: crate::config::RecordingType::Sipflow,
        stereo_swap: false,
    };
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto)
        .with_application("ivr".to_string(), None, true)
        .with_recording(recording);
    let mut session = build_session_on_server(server, dialplan).await;

    setup_recording_test_media(&mut session).await;
    let packet = rustrtc::rtp::RtpPacket::new(
        rustrtc::rtp::RtpHeader::new(0, 1, 160, 1234),
        vec![0xff; 160],
    );
    session
        .media
        .bridge
        .as_ref()
        .expect("media bridge")
        .leg(crate::media::media_bridge::LegSide::A)
        .expect("caller leg")
        .ingress_tap()
        .on_ingress(&packet, "127.0.0.1:40000".parse().unwrap());

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while backend.rtp_records.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("SipFlow recorder did not receive RTP after media setup");

    if let Some(mut bridge) = session.media.bridge.take() {
        bridge
            .stop_recording()
            .await
            .expect("stop SipFlow recorder");
        bridge.close();
    }
}

#[tokio::test]
async fn sipflow_recording_auto_start_false_keeps_backend_idle() {
    use rustrtc::peer_connection::RtpObserver;

    let backend = Arc::new(CountingSipflowBackend {
        rtp_records: AtomicUsize::new(0),
    });
    let (server, _) = create_test_server_with_config_and_sipflow_backend(
        ProxyConfig::default(),
        Some(backend.clone()),
    )
    .await;
    let recording = crate::call::CallRecordingConfig {
        enabled: true,
        option: None,
        auto_start: false,
        auto_start_at: crate::config::RecordingAutoStartAt::Media,
        recording_type: crate::config::RecordingType::Sipflow,
        stereo_swap: false,
    };
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto)
        .with_application("ivr".to_string(), None, true)
        .with_recording(recording);
    let mut session = build_session_on_server(server, dialplan).await;

    setup_recording_test_media(&mut session).await;
    let bridge = session.media.bridge.as_ref().expect("media bridge");
    assert!(
        bridge.has_recorder_task(),
        "enabled manual recording must prepare the capture sender/task"
    );
    let packet = rustrtc::rtp::RtpPacket::new(
        rustrtc::rtp::RtpHeader::new(0, 1, 160, 1234),
        vec![0xff; 160],
    );
    bridge
        .leg(crate::media::media_bridge::LegSide::A)
        .expect("caller leg")
        .ingress_tap()
        .on_ingress(&packet, "127.0.0.1:40000".parse().unwrap());
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert_eq!(
        backend.rtp_records.load(Ordering::SeqCst),
        0,
        "auto_start=false must not activate the SipFlow backend"
    );

    if let Some(mut bridge) = session.media.bridge.take() {
        bridge.stop_recording().await.expect("stop idle recorder");
        bridge.close();
    }
}

#[tokio::test]
async fn test_session_captures_rewritten_dialplan_uris_for_call_record() {
    let caller = rsipstack::sip::Uri::try_from("sip:rewritten-caller@source.example.com").unwrap();
    let callee = rsipstack::sip::Uri::try_from("sip:001234@carrier.example.com:5060").unwrap();
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto)
        .with_caller(caller)
        .with_targets(DialStrategy::Sequential(vec![crate::call::Location {
            aor: callee,
            ..Default::default()
        }]));

    let session = build_session(dialplan).await;
    let snapshot = session.record_snapshot();

    assert_eq!(
        snapshot.routed_caller.as_deref(),
        Some("sip:rewritten-caller@source.example.com")
    );
    assert_eq!(
        snapshot.routed_callee.as_deref(),
        Some("sip:001234@carrier.example.com:5060")
    );
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
        .start_ivr_app("hello", HashMap::new())
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
        .start_ivr_app("hello", HashMap::new())
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
        .start_ivr_app("hello", HashMap::new())
        .await
        .expect_err("non-AlreadyRunning error should be returned");
    assert!(
        err.to_string().contains("Failed to start IVR 'hello'"),
        "unexpected error: {}",
        err
    );
}

/// When `start_app` fails in the Application flow (e.g. a missing IVR config
/// file makes the factory return `None`), `execute_dialplan` must surface a
/// structured failure: set the standardized `ivr.start_failed` error code,
/// append an Error-level Ivr trace event, and return a 5xx rejection so the
/// caller hears the configured failure tone (`RingbackAudio::error`) instead
/// of ringing until the ring-timeout. Regression for the silent-Ok bug where
/// the failure was swallowed and the caller hung for 60s on a 408.
#[tokio::test]
async fn test_execute_dialplan_application_start_failure_sets_error_code_and_trace() {
    use rsipstack::dialog::dialog::DialogState;

    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_application(
        "ivr".to_string(),
        None,
        true,
    );
    let mut session = build_session(dialplan).await;
    session.app_runtime = Arc::new(AlwaysFailStartRuntime);

    let (_tx, mut rx) = mpsc::unbounded_channel::<DialogState>();
    let err = session
        .execute_dialplan(&mut rx)
        .await
        .expect_err("application start failure should surface as a dialplan error");
    assert_eq!(
        err.0, 500,
        "expected 500 rejection for IVR start failure, got {}",
        err.0
    );

    assert_eq!(
        session.meta.error_code.map(|i| i.code),
        Some("ivr.start_failed"),
        "IVR start failure must record the ivr.start_failed error code"
    );

    let ivr_err = session
        .meta
        .trace
        .iter()
        .find(|e| {
            e.kind == crate::call_errors::TraceKind::Ivr
                && e.severity == Some(crate::call_errors::ErrSeverity::Error)
        })
        .expect("an Error-level Ivr trace event must be appended on start failure");
    assert_eq!(ivr_err.code.as_deref(), Some("ivr.start_failed"));

    assert!(
        !session
            .meta
            .trace
            .iter()
            .any(|e| e.message.contains("started")),
        "the misleading 'Application started' Info trace must NOT be recorded on failure"
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
        .start_ivr_app("hello", HashMap::new())
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
            ..Default::default()
        }),
        ..RouteQueueConfig::default()
    };

    let plan = queue.to_queue_plan().expect("queue plan should build");
    match plan.fallback {
        Some(QueueFallbackAction::Failure(FailureAction::Hangup { .. })) => {}
        other => panic!("expected Hangup fallback, got {:?}", other),
    }
}

#[tokio::test]
async fn test_queue_transfer_without_return_to_ivr_starts_queue_app() {
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
        .handle_queue_transfer("support", None, Vec::new(), None)
        .await
        .expect("queue app should start");
    assert_eq!(runtime.start_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_queue_transfer_return_to_ivr_starts_queue_app_and_sets_meta() {
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
            "support",
            Some(crate::proxy::proxy_call::sip_session::ReturnTargetSpec {
                app_name: "ivr".to_string(),
                target: Some("hello".to_string()),
                params: HashMap::new(),
            }),
            Vec::new(),
            None,
        )
        .await
        .expect("queue app should start with return_app");

    assert_eq!(runtime.start_calls.load(Ordering::SeqCst), 1);
    assert!(session.meta.transfer_return_app.is_some());
    assert_eq!(
        session.meta.transfer_return_app.as_ref().unwrap().app_name,
        "ivr"
    );
}

// ─── accept_call connected_callee regression tests ───────────────────────────

/// accept_call must set connected_callee for a plain P2P (Targets, no bridge) call.
///
/// Regression: the fix must not prevent connected_callee from being assigned.
#[tokio::test]
async fn test_accept_call_sets_connected_callee_for_p2p_targets_flow() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto)
        .with_targets(crate::call::DialStrategy::Sequential(vec![]));
    let mut session = build_session(dialplan).await;

    session
        .accept_call(Some("sip:bob@rustpbx.com".to_string()), None)
        .await
        .expect("accept_call should succeed for P2P call");

    assert_eq!(
        session.meta.connected_callee,
        Some("sip:bob@rustpbx.com".to_string()),
        "connected_callee must be set after accept_call"
    );
}

/// accept_call must set connected_callee for an Application-flow (IVR/Queue) call.
#[tokio::test]
async fn test_accept_call_sets_connected_callee_for_application_ivr_flow() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_application(
        "ivr".to_string(),
        None,
        true,
    );
    let mut session = build_session(dialplan).await;

    session
        .accept_call(Some("sip:agent@rustpbx.com".to_string()), None)
        .await
        .expect("accept_call should succeed for IVR flow");

    assert_eq!(
        session.meta.connected_callee,
        Some("sip:agent@rustpbx.com".to_string()),
        "connected_callee must be set after accept_call in IVR/Application flow"
    );
}

/// For bridge-based calls (Targets flow, e.g. wholesale with WebRTC caller),
/// accept_call() must complete without panic and set connected_callee.
#[tokio::test]
async fn test_accept_call_for_bridge_wholesale_flow_sets_connected_callee() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto)
        .with_targets(crate::call::DialStrategy::Sequential(vec![]));
    let mut session = build_session(dialplan).await;

    session
        .accept_call(Some("sip:trunk@wholesale.example".to_string()), None)
        .await
        .expect("accept_call should succeed for bridge-based wholesale call");

    assert_eq!(
        session.meta.connected_callee,
        Some("sip:trunk@wholesale.example".to_string()),
        "connected_callee must be set after accept_call for bridge-based call"
    );
}

/// Calling accept_call twice (re-INVITE / transfer scenario) must succeed
/// without panic, and connected_callee must be updated to the new value.
#[tokio::test]
async fn test_accept_call_twice_updates_connected_callee() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto)
        .with_targets(crate::call::DialStrategy::Sequential(vec![]));
    let mut session = build_session(dialplan).await;

    // First accept — callee A
    session
        .accept_call(Some("sip:a@example.com".to_string()), None)
        .await
        .expect("first accept_call should succeed");
    assert_eq!(
        session.meta.connected_callee,
        Some("sip:a@example.com".to_string())
    );

    // Second accept — callee B (re-INVITE / transfer scenario).
    session
        .accept_call(Some("sip:b@example.com".to_string()), None)
        .await
        .expect("second accept_call should not panic or fail");
    assert_eq!(
        session.meta.connected_callee,
        Some("sip:b@example.com".to_string())
    );
}

// ─── Call-trace answer/agent regression tests ────────────────────────────────

/// A repeated self/app answer (e.g. `auto_answer` on app start combined with
/// the app's own `ctrl.answer()` in `on_enter`) must not append a second
/// "Call answered" trace event.
#[tokio::test]
async fn test_accept_call_duplicate_app_answer_records_single_answer_trace() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_application(
        "ivr".to_string(),
        None,
        true,
    );
    let mut session = build_session(dialplan).await;

    session
        .accept_call(None, None)
        .await
        .expect("first app accept_call should succeed");
    session
        .accept_call(None, None)
        .await
        .expect("duplicate app accept_call should be a no-op");

    let answers: Vec<_> = session
        .meta
        .trace
        .iter()
        .filter(|e| e.kind == crate::call_errors::TraceKind::Answer)
        .collect();
    assert_eq!(
        answers.len(),
        1,
        "duplicate app answer must not append a second Answer trace"
    );
}

/// An agent/callee answer must still record an Answer trace and enrich it with
/// agent identity (resolved_agent_id or callee user-part) and queue context.
#[tokio::test]
async fn test_accept_call_agent_answer_trace_carries_agent_detail() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_application(
        "queue".to_string(),
        None,
        false,
    );
    let mut session = build_session(dialplan).await;

    session.meta.queue_name = Some("support".to_string());
    {
        let mut ext = session.extensions.write();
        let mut map: HashMap<String, String> = HashMap::new();
        map.insert("resolved_agent_id".to_string(), "1001".to_string());
        ext.insert(map);
    }

    session
        .accept_call(Some("sip:1001@rustpbx.com".to_string()), None)
        .await
        .expect("agent accept_call should succeed");

    let answer = session
        .meta
        .trace
        .iter()
        .find(|e| e.kind == crate::call_errors::TraceKind::Answer)
        .expect("agent answer should record an Answer trace");
    let detail = answer
        .detail
        .as_ref()
        .expect("agent answer trace should carry detail");
    assert_eq!(detail["agent_id"].as_str(), Some("1001"));
    assert_eq!(detail["callee"].as_str(), Some("sip:1001@rustpbx.com"));
    assert_eq!(detail["queue_name"].as_str(), Some("support"));
}

/// The terminal End trace event must carry a real elapsed timestamp, not the
/// default 0 (which rendered as "+0ms" in the console UI).
#[tokio::test]
async fn test_record_snapshot_end_trace_has_real_timestamp() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto);
    let mut session = build_session(dialplan).await;

    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    session.meta.hangup_reason = Some(crate::callrecord::CallRecordHangupReason::ByCaller);

    let snapshot = session.record_snapshot();
    let trace = snapshot
        .metadata
        .get("trace")
        .expect("snapshot should carry a trace array")
        .as_array()
        .expect("trace must be an array");
    let end = trace
        .iter()
        .find(|e| e["kind"] == "end")
        .expect("trace should contain an End event");
    let ts = end["ts"].as_i64().expect("End event must have ts");
    assert!(ts > 0, "End event ts must be non-zero, got {ts}");
}

/// Queue abandon refinement must fire even when the hangup reason was already
/// normalized to `Abandoned` (e.g. by `execute_queue`), so the CDR carries
/// `queue.abandoned` instead of a leaked "all agents unavailable" code.
#[tokio::test]
async fn test_resolve_final_hangup_reason_flags_queue_abandon_when_already_abandoned() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto);
    let mut session = build_session(dialplan).await;

    session.meta.queue_name = Some("support".to_string());
    session.meta.hangup_reason = Some(crate::callrecord::CallRecordHangupReason::Abandoned);
    // Mirror the resolved agent from the queue app's target resolution.
    {
        use std::collections::HashMap;
        let mut ext = session.extensions.write();
        let mut map = HashMap::new();
        map.insert("resolved_agent_id".to_string(), "1001".to_string());
        ext.insert(map);
    }

    session.resolve_final_hangup_reason().await;

    assert_eq!(
        session.meta.error_code.map(|info| info.code),
        Some("queue.abandoned"),
        "queue abandon must set queue.abandoned error code"
    );
    assert!(
        session
            .meta
            .trace
            .iter()
            .any(|e| e.code.as_deref() == Some("queue.abandoned")),
        "abandon must append a queue.abandoned trace event"
    );

    // The abandon trace must name the queue and carry it in the detail.
    let abandon = session
        .meta
        .trace
        .iter()
        .find(|e| e.code.as_deref() == Some("queue.abandoned"))
        .expect("queue.abandoned trace present");
    assert!(
        abandon.message.contains("support"),
        "abandon trace must name the queue, got: {}",
        abandon.message
    );
    assert_eq!(
        abandon
            .detail
            .as_ref()
            .and_then(|d| d.get("queue_name"))
            .and_then(|v| v.as_str()),
        Some("support"),
        "abandon trace detail must carry queue_name"
    );
    assert_eq!(
        abandon
            .detail
            .as_ref()
            .and_then(|d| d.get("agent"))
            .and_then(|v| v.as_str()),
        Some("1001"),
        "abandon trace detail must carry resolved agent id"
    );
}

/// A leg failure while the call is being driven by the queue must record an
/// agent-rejection trace that names the agent, the SIP status and the queue —
/// so the operator sees *why* the queue could not connect (e.g. 486 from an
/// off-hours / time-of-day rejection) instead of only a generic abandon.
#[tokio::test]
async fn test_leg_failed_in_queue_records_agent_rejection_trace() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_queue(QueuePlan {
        queue_name: "support".to_string(),
        ..Default::default()
    });
    let mut session = build_session(dialplan).await;
    session.meta.queue_name = Some("support".to_string());

    // Mirror custom-target resolution: agent AOR user part in session extensions.
    let mut map = HashMap::new();
    map.insert("resolved_agent_id".to_string(), "1001".to_string());
    session.extensions.write().insert(map);

    let agent_leg = LegId::from("queue-agent");
    let leg = Leg::new(agent_leg.clone())
        .with_endpoint("sip:3tmpv1bu@agent.invalid;transport=WS".to_string());
    session.legs.insert(agent_leg.clone(), leg);

    session
        .execute_command(
            CallCommand::LegFailed {
                leg_id: agent_leg,
                reason: "Rejected with 486".to_string(),
            },
            None,
        )
        .await;

    let ev = session
        .meta
        .trace
        .iter()
        .find(|e| {
            e.kind == crate::call_errors::TraceKind::Queue && e.message.contains("Agent 1001")
        })
        .expect("agent rejection should be recorded in the trace");
    assert_eq!(
        ev.message, "Agent 1001 rejected (486)",
        "rejection trace must name the agent and the SIP status"
    );
    let detail = ev
        .detail
        .as_ref()
        .expect("rejection trace should carry detail");
    assert_eq!(detail["agent"].as_str(), Some("1001"));
    assert_eq!(detail["status"].as_str(), Some("486"));
    assert_eq!(detail["queue_name"].as_str(), Some("support"));
}

/// A leg no-answer while queued must be recorded as an agent no-answer trace.
#[tokio::test]
async fn test_leg_no_answer_in_queue_records_agent_trace() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_queue(QueuePlan {
        queue_name: "support".to_string(),
        ..Default::default()
    });
    let mut session = build_session(dialplan).await;
    session.meta.queue_name = Some("support".to_string());

    let mut map = HashMap::new();
    map.insert("resolved_agent_id".to_string(), "1001".to_string());
    session.extensions.write().insert(map);

    let agent_leg = LegId::from("queue-agent");
    let leg = Leg::new(agent_leg.clone())
        .with_endpoint("sip:3tmpv1bu@agent.invalid;transport=WS".to_string());
    session.legs.insert(agent_leg.clone(), leg);

    session
        .execute_command(
            CallCommand::LegFailed {
                leg_id: agent_leg,
                reason: "Timeout".to_string(),
            },
            None,
        )
        .await;

    let ev = session
        .meta
        .trace
        .iter()
        .find(|e| {
            e.kind == crate::call_errors::TraceKind::Queue && e.message.contains("Agent 1001")
        })
        .expect("agent no-answer should be recorded in the trace");
    assert_eq!(ev.message, "Agent 1001 no answer");
}

/// A caller hangup after the call was already served by an agent must NOT be
/// classified as a queue abandon — even though the agent leg has since
/// terminated (connected_callee cleared) and queue_name is still set. This
/// reproduces the IVR → queue → agent answered → agent hung up → return IVR →
/// caller hung up flow (see call fgkou895n0g5g0751g5v in dev.log).
#[tokio::test]
async fn test_resolve_final_hangup_reason_no_abandon_after_agent_connected() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto);
    let mut session = build_session(dialplan).await;

    session.meta.queue_name = Some("to-agent".to_string());
    session.meta.hangup_reason = Some(crate::callrecord::CallRecordHangupReason::ByCaller);
    session.meta.ever_connected_callee = true;

    session.resolve_final_hangup_reason().await;

    assert_ne!(
        session.meta.error_code.map(|info| info.code),
        Some("queue.abandoned"),
        "caller was served by an agent; must not be flagged as queue abandoned"
    );
    assert!(
        !session
            .meta
            .trace
            .iter()
            .any(|e| e.code.as_deref() == Some("queue.abandoned")),
        "served call must not append a queue.abandoned trace event"
    );
    assert_eq!(
        session.meta.hangup_reason,
        Some(crate::callrecord::CallRecordHangupReason::ByCaller),
        "hangup reason must stay ByCaller after a served queue interaction"
    );
}

/// A stale error_code from earlier in the call (e.g. a recovered queue error)
/// must not leak into the call trace as "IVR ended: …" when the IVR ended via
/// a continuation value (cancelled / transferred / chained).
#[tokio::test]
async fn test_resolve_final_hangup_reason_no_stale_ivr_end_trace() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto);
    let mut session = build_session(dialplan).await;

    session.meta.error_code = Some(&crate::proxy::proxy_call::error_catalog::QUEUE_ABANDONED);
    let ctx = session
        .app_runtime
        .app_context()
        .expect("test session uses DefaultAppRuntime")
        .clone();
    ctx.set_var("ivr_end_reason", "cancelled");

    session.resolve_final_hangup_reason().await;

    assert!(
        !session
            .meta
            .trace
            .iter()
            .any(|e| e.kind == crate::call_errors::TraceKind::Ivr
                && e.code.as_deref() == Some("queue.abandoned")),
        "IVR ended via continuation (cancelled) must not surface a stale queue.abandoned trace"
    );
}

/// Starting the queue app must record a "Entered queue '<name>'" trace event
/// so the call trace correctly shows every lifecycle transition (IVR →
/// Entered queue → caller abandoned → end) instead of jumping straight from
/// "answered" to "queue.abandoned".
#[tokio::test]
async fn test_start_queue_app_records_queue_entry_trace_and_app_name() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto);
    let mut config = ProxyConfig::default();
    config.queues.insert(
        "support".to_string(),
        RouteQueueConfig {
            name: Some("support".to_string()),
            strategy: RouteQueueStrategyConfig {
                targets: vec![RouteQueueTargetConfig {
                    uri: "skill-group:nonexistent".to_string(),
                    label: Some("no-agents".to_string()),
                }],
                ..Default::default()
            },
            ..RouteQueueConfig::default()
        },
    );
    let mut session = build_session_with_config(dialplan, config).await;

    let runtime = Arc::new(StartOnlyRuntime::new());
    session.app_runtime = runtime.clone();

    session
        .handle_queue_transfer("support", None, Vec::new(), None)
        .await
        .expect("queue app should start");

    let entered = session
        .meta
        .trace
        .iter()
        .find(|e| {
            e.kind == crate::call_errors::TraceKind::Queue && e.message == "Entered queue 'support'"
        })
        .expect("queue entry must be recorded in the trace");
    assert!(
        entered.code.is_none(),
        "queue entry is informational, not an error"
    );
    assert_eq!(
        entered
            .detail
            .as_ref()
            .and_then(|d| d.get("queue_name"))
            .and_then(|v| v.as_str()),
        Some("support"),
        "queue entry detail must carry queue_name"
    );
    assert_eq!(
        session.meta.app_name.as_deref(),
        Some("queue"),
        "terminal phase must be attributed to the queue app, not a stale IVR"
    );
}

/// `session_hook_ctx().queue_name` feeds cc_* RWI webhook events (`queue_id`
/// field). It must be populated from CallMeta when the queue app starts — not
/// deferred to QueueApp::on_enter's old ApplicationContext mirror.
#[tokio::test]
async fn test_queue_start_populates_meta_for_webhook_hooks() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto);
    let mut config = ProxyConfig::default();
    config.queues.insert(
        "support".to_string(),
        RouteQueueConfig {
            name: Some("support".to_string()),
            strategy: RouteQueueStrategyConfig {
                targets: vec![RouteQueueTargetConfig {
                    uri: "skill-group:sg_support".to_string(),
                    label: Some("support-group".to_string()),
                }],
                ..Default::default()
            },
            ..RouteQueueConfig::default()
        },
    );
    let mut session = build_session_with_config(dialplan, config).await;

    let runtime = Arc::new(StartOnlyRuntime::new());
    session.app_runtime = runtime.clone();

    session
        .handle_queue_transfer("support", None, Vec::new(), None)
        .await
        .expect("queue app should start");

    assert_eq!(
        session.meta.queue_name.as_deref(),
        Some("support"),
        "CallMeta.queue_name must be set before cc_* / abandon hooks fire"
    );
    assert_eq!(
        session.meta.skill_group_id.as_deref(),
        Some("sg_support"),
        "CallMeta.skill_group_id must be set for CSAT/wrapup webhook lookups"
    );
    assert_eq!(
        crate::proxy::proxy_call::call_meta::effective_queue_name(&session.meta).as_deref(),
        Some("support"),
        "effective_queue_name is what session_hook_ctx exposes to webhooks"
    );
}

// ── parse_info_command pure function tests ──

#[test]
fn parse_hold() {
    let parsed = serde_json::json!({"action":"hold","params":{"leg_id":"caller"}});
    let cmd = SipSession::parse_info_command("hold", parsed.get("params"), &parsed);
    assert!(matches!(cmd, Some(CallCommand::Hold { leg_id, .. }) if leg_id.as_str() == "caller"));
}

#[test]
fn parse_hold_with_music() {
    let parsed = serde_json::json!({
        "action":"hold",
        "params":{"leg_id":"caller","music":{"source_type":"file","uri":"music.wav"}}
    });
    let cmd = SipSession::parse_info_command("hold", parsed.get("params"), &parsed);
    assert!(
        matches!(cmd, Some(CallCommand::Hold { leg_id, music: Some(_), .. }) if leg_id.as_str() == "caller")
    );
}

#[test]
fn parse_hold_default_leg() {
    let parsed = serde_json::json!({"action":"hold","params":{}});
    let cmd = SipSession::parse_info_command("hold", parsed.get("params"), &parsed);
    assert!(matches!(cmd, Some(CallCommand::Hold { leg_id, .. }) if leg_id.as_str() == "caller"));
}

#[test]
fn parse_unhold() {
    let parsed = serde_json::json!({"action":"unhold","params":{"leg_id":"callee"}});
    let cmd = SipSession::parse_info_command("unhold", parsed.get("params"), &parsed);
    assert!(matches!(cmd, Some(CallCommand::Unhold { leg_id }) if leg_id.as_str() == "callee"));
}

#[test]
fn parse_media_play() {
    let parsed = serde_json::json!({
        "action":"media.play",
        "params":{"source":{"source_type":"file","uri":"prompt.wav"},"loop":true}
    });
    let cmd = SipSession::parse_info_command("media.play", parsed.get("params"), &parsed);
    assert!(matches!(cmd, Some(CallCommand::Play { .. })));
}

#[test]
fn parse_media_stop() {
    let parsed = serde_json::json!({"action":"media.stop","params":{"leg_id":"caller"}});
    let cmd = SipSession::parse_info_command("media.stop", parsed.get("params"), &parsed);
    assert!(matches!(cmd, Some(CallCommand::StopPlayback { .. })));
}

#[test]
fn parse_record_start() {
    let parsed = serde_json::json!({
        "action":"record.start",
        "params":{"path":"/tmp/rec.wav","beep":true}
    });
    let cmd = SipSession::parse_info_command("record.start", parsed.get("params"), &parsed);
    assert!(matches!(cmd, Some(CallCommand::StartRecording { .. })));
}

#[test]
fn parse_record_stop() {
    let parsed = serde_json::json!({"action":"record.stop","params":{}});
    let cmd = SipSession::parse_info_command("record.stop", parsed.get("params"), &parsed);
    assert!(matches!(cmd, Some(CallCommand::StopRecording)));
}

#[test]
fn parse_consult_initiate() {
    let parsed = serde_json::json!({
        "action":"consult.initiate",
        "params":{"leg_id":"caller"}
    });
    let cmd = SipSession::parse_info_command("consult.initiate", parsed.get("params"), &parsed);
    assert!(matches!(cmd, Some(CallCommand::Hold { .. })));
}

#[test]
fn parse_consult_cancel() {
    let parsed = serde_json::json!({
        "action":"consult.cancel",
        "params":{"leg_id":"caller"}
    });
    let cmd = SipSession::parse_info_command("consult.cancel", parsed.get("params"), &parsed);
    assert!(matches!(cmd, Some(CallCommand::Unhold { .. })));
}

#[test]
fn parse_unknown_returns_none() {
    let parsed = serde_json::json!({"action":"nonexistent","params":{}});
    let cmd = SipSession::parse_info_command("nonexistent", parsed.get("params"), &parsed);
    assert!(cmd.is_none());
}

#[tokio::test]
async fn test_session_drop_releases_all_grouped_concurrent_call_permits() {
    let first = crate::call::concurrent_call_limiter::ConcurrentCallLimiter::new(1);
    let second = crate::call::concurrent_call_limiter::ConcurrentCallLimiter::new(1);
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto);
    dialplan
        .concurrent_call_lease
        .push(first.try_acquire().unwrap());
    dialplan
        .concurrent_call_lease
        .push(second.try_acquire().unwrap());
    let session = build_session(dialplan).await;
    assert!(
        session.context.dialplan.concurrent_call_lease.is_empty(),
        "session construction must take the permits out of the dialplan"
    );
    assert_eq!(session.concurrent_call_lease.len(), 2);
    assert_eq!(first.current(), 1);
    assert_eq!(second.current(), 1);

    drop(session);

    assert_eq!(first.current(), 0);
    assert_eq!(second.current(), 0);
}

// ─── handle_play await_completion regression ────────────────────────────────
//
// Regression for the bug where `handle_play` discarded `await_completion`,
// causing queue transfer/failure prompts (and any awaited prompt) to be cut
// off the instant playback started. We wire a REAL MediaBridge (negotiated
// RTP legs playing a short temp WAV) into a test SipSession and verify the
// await flag actually blocks until the file finishes.

/// Write `num_samples` of 16-bit PCM mono silence at `sample_rate` Hz to a
/// minimal WAV file and return its path.
fn write_silence_wav(
    dir: &std::path::Path,
    name: &str,
    sample_rate: u32,
    num_samples: u32,
) -> std::path::PathBuf {
    use std::io::Write;
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).expect("create wav");
    let data_size = num_samples * 2u32; // 16-bit mono
    let riff_size = 36 + data_size;
    f.write_all(b"RIFF").unwrap();
    f.write_all(&riff_size.to_le_bytes()).unwrap();
    f.write_all(b"WAVE").unwrap();
    f.write_all(b"fmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap(); // subchunk1 size
    f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
    f.write_all(&1u16.to_le_bytes()).unwrap(); // mono
    f.write_all(&sample_rate.to_le_bytes()).unwrap();
    f.write_all(&(sample_rate * 2).to_le_bytes()).unwrap(); // byte rate
    f.write_all(&2u16.to_le_bytes()).unwrap(); // block align
    f.write_all(&16u16.to_le_bytes()).unwrap(); // bits per sample
    f.write_all(b"data").unwrap();
    f.write_all(&data_size.to_le_bytes()).unwrap();
    f.write_all(&vec![0u8; data_size as usize]).unwrap();
    path
}

/// Build a real, single-leg-A negotiated MediaBridge suitable for `play_file`.
async fn playable_bridge(session_id: &str) -> crate::media::media_bridge::MediaBridge {
    use crate::media::leg::{LegConfig, LegInner};
    use crate::media::media_bridge::{LegSide, MediaBridge};
    let mut mb = MediaBridge::new(session_id);
    let recorder_sender = mb.setup_recorder_task().expect("recording task");
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), Some(recorder_sender)).expect("leg a");
    let b = LegInner::new("b", &LegConfig::rtp_pcmu(), None).expect("leg b");
    mb.replace_leg(LegSide::A, a).await;
    mb.replace_leg(LegSide::B, b).await;
    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();
    let offer = la.create_offer().await.expect("offer");
    let answer = lb.answer(&offer).await.expect("answer");
    la.apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .expect("apply answer");
    mb
}

#[tokio::test]
async fn handle_play_awaits_completion_when_requested() {
    let dir = tempfile::tempdir().expect("tempdir");
    // 300ms of silence @8kHz.
    let wav = write_silence_wav(dir.path(), "prompt.wav", 8000, 8000 * 300 / 1000);

    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_application(
        "ivr".to_string(),
        None,
        true,
    );
    let mut session = build_session(dialplan).await;
    session.media.bridge = Some(playable_bridge("await-true").await);

    let start = Instant::now();
    session
        .handle_play(
            None,
            crate::call::domain::MediaSource::File {
                path: wav.to_str().unwrap().to_string(),
            },
            Some(crate::call::domain::PlayOptions {
                await_completion: true,
                loop_playback: false,
                ..Default::default()
            }),
        )
        .await
        .expect("play should succeed");
    let elapsed = start.elapsed();

    // With the fix, the call blocks until the ~300ms prompt finishes.
    // Without the fix this was ~0ms (prompt cut off instantly).
    assert!(
        elapsed >= std::time::Duration::from_millis(200),
        "await_completion=true should block until the prompt finishes, took {:?}",
        elapsed
    );
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "playback should not take that long: {:?}",
        elapsed
    );
}

#[tokio::test]
async fn handle_play_returns_immediately_when_not_awaited() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wav = write_silence_wav(dir.path(), "prompt.wav", 8000, 8000 * 300 / 1000);

    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_application(
        "ivr".to_string(),
        None,
        true,
    );
    let mut session = build_session(dialplan).await;
    session.media.bridge = Some(playable_bridge("await-false").await);

    let start = Instant::now();
    session
        .handle_play(
            None,
            crate::call::domain::MediaSource::File {
                path: wav.to_str().unwrap().to_string(),
            },
            Some(crate::call::domain::PlayOptions {
                await_completion: false,
                loop_playback: false,
                ..Default::default()
            }),
        )
        .await
        .expect("play should succeed");
    let elapsed = start.elapsed();

    // Fire-and-forget: returns right away (the pacing task runs in background).
    assert!(
        elapsed < std::time::Duration::from_millis(150),
        "await_completion=false should return immediately, took {:?}",
        elapsed
    );
}

// ─── queue agent connect activates the caller↔agent media bridge ─────────────
//
// Regression: when a queue agent answers (dynamic leg), the MediaBridge B leg
// is created with only a local offer (create_callee_track) and the agent's
// answer SDP was never applied to it, and neither leg was accepted / bridged.
// So the caller and agent both showed "connected" but no audio flowed in
// either direction (the recording captured only hold music / silence).
// The `LegConnected` handler must apply the answer SDP, accept both legs and
// activate the bridge.

#[tokio::test]
async fn queue_agent_connect_activates_media_bridge() {
    use crate::media::leg::{LegConfig, LegInner};

    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_queue(QueuePlan {
        queue_name: "support".to_string(),
        ..Default::default()
    });
    let mut session = build_session(dialplan).await;

    // Caller side: a valid PCMU offer as the inbound INVITE body.
    let caller_offer = crate::proxy::tests::test_helpers::pcmu_sdp("127.0.0.1", 10001);
    session.media.caller_offer = Some(caller_offer);

    // `create_callee_track` builds the B leg on the MediaBridge and returns the
    // agent-offer SDP that would be sent in the queue agent INVITE.
    let agent_offer = session
        .create_callee_track(false)
        .await
        .expect("create callee track");
    assert!(
        agent_offer.contains("m=audio"),
        "agent offer must carry audio m-line"
    );

    // Simulate the agent answering: build a scratch RTP/PCMU leg to answer the
    // offer, yielding the agent's answer SDP.
    let agent_scratch = LegInner::new("agent-scratch", &LegConfig::rtp_pcmu(), None).unwrap();
    let agent_answer = agent_scratch
        .answer(&agent_offer)
        .await
        .expect("agent answer");
    assert!(
        agent_answer.contains("m=audio"),
        "agent answer must carry audio m-line"
    );

    // Register the dynamic queue-agent leg, then feed LegConnected.
    let agent_leg = LegId::from("queue-agent-1");
    session.legs.insert(
        agent_leg.clone(),
        Leg::new(agent_leg.clone()).with_endpoint("sip:1002@127.0.0.1"),
    );
    session
        .execute_command(
            CallCommand::LegConnected {
                leg_id: agent_leg,
                answer_sdp: Some(agent_answer),
                dialog_id: None,
            },
            None,
        )
        .await;

    // The media bridge must now be active (both legs accepted + relay armed).
    let mb = session.media.bridge.as_ref().expect("media bridge present");
    assert!(
        mb.is_bridged(),
        "queue agent connect must activate the caller<->agent media bridge"
    );

    if let Some(mb) = session.media.bridge.as_mut() {
        mb.close();
    }
}

// ─── proxy queue fallback routing (no agents) ───────────────────────────────
//
// Covers the path the original bug report exercised: IVR → queue transfer →
// skill-group with no registered agents → fallback. Verifies the configured
// fallback action is taken. (MockMediaPeer has no media, so play_audio_file
// errors out harmlessly inside the fallback — we assert on control flow.)

/// Build a queue config whose skill-group target resolves to zero agents,
/// plus a given fallback configuration.
fn make_queue_config_with_fallback(
    queue_name: &str,
    fallback: RouteQueueFallbackConfig,
) -> ProxyConfig {
    let mut config = ProxyConfig::default();
    config.queues.insert(
        queue_name.to_string(),
        RouteQueueConfig {
            name: Some(queue_name.to_string()),
            strategy: RouteQueueStrategyConfig {
                targets: vec![RouteQueueTargetConfig {
                    uri: "skill-group:nonexistent".to_string(),
                    label: Some("no-agents".to_string()),
                }],
                ..Default::default()
            },
            fallback: Some(fallback),
            ..Default::default()
        },
    );
    config
}

#[tokio::test]
async fn queue_no_agents_play_then_hangup_starts_queue_app() {
    // Queue with no reachable agents and default fallback → queue app starts
    // (the app handles the fallback asynchronously).
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_application(
        "ivr".to_string(),
        None,
        true,
    );
    let mut config = ProxyConfig::default();
    config.queues.insert(
        "db-1".to_string(),
        RouteQueueConfig {
            name: Some("to-agent".to_string()),
            strategy: RouteQueueStrategyConfig {
                targets: vec![RouteQueueTargetConfig {
                    uri: "skill-group:nonexistent".to_string(),
                    label: Some("no-agents".to_string()),
                }],
                ..Default::default()
            },
            ..RouteQueueConfig::default()
        },
    );
    let mut session = build_session_with_config(dialplan, config).await;

    let runtime = Arc::new(StartOnlyRuntime::new());
    session.app_runtime = runtime.clone();

    session
        .handle_queue_transfer("support", None, Vec::new(), None)
        .await
        .expect("queue app should start");
    assert_eq!(runtime.start_calls.load(Ordering::SeqCst), 1);
}

// ─── IVR → queue transfer when the queue does not exist ─────────────────────
//
// Regression for the dead-air bug: when an IVR's Queue action points at a
// queue whose config cannot be resolved (e.g. a DB-backed id that isn't
// loaded), `handle_queue_transfer` used to return a bare error that the
// session loop only logged as a WARN. The caller heard nothing and the call
// was never torn down. The fix records a `queue.not_found` trace event and
// applies a graceful fallback: start the `return_to_ivr` IVR if set, else
// start the queue app with a service-unavailable announcement + hangup plan.

#[tokio::test]
async fn queue_not_found_with_return_to_ivr_starts_ivr_app() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_application(
        "ivr".to_string(),
        None,
        true,
    );
    // No queues configured → "asdf-queue" cannot be resolved.
    let mut session = build_session(dialplan).await;

    let runtime = Arc::new(NameCapturingRuntime::new());
    session.app_runtime = runtime.clone();

    session
        .handle_queue_transfer(
            "asdf-queue",
            Some(crate::proxy::proxy_call::sip_session::ReturnTargetSpec {
                app_name: "ivr".to_string(),
                target: Some("asdf".to_string()),
                params: HashMap::new(),
            }),
            Vec::new(),
            None,
        )
        .await
        .expect("missing-queue fallback with return_app should start the IVR app");

    // The IVR app (not the queue app) must be started.
    assert_eq!(
        runtime.started_apps(),
        vec!["ivr".to_string()],
        "return_to_ivr should restart the IVR, not the queue app"
    );

    // A queue.not_found trace event must be recorded.
    let has_trace = session.meta.trace.iter().any(|ev| {
        ev.kind == crate::call_errors::TraceKind::Queue
            && ev.code.as_deref() == Some("queue.not_found")
    });
    assert!(
        has_trace,
        "expected a queue.not_found trace event; trace = {:?}",
        session.meta.trace
    );
}

#[tokio::test]
async fn queue_not_found_without_return_to_ivr_starts_queue_app() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_application(
        "ivr".to_string(),
        None,
        true,
    );
    // No queues configured → "missing-queue" cannot be resolved.
    let mut session = build_session(dialplan).await;

    let runtime = Arc::new(NameCapturingRuntime::new());
    session.app_runtime = runtime.clone();

    session
        .handle_queue_transfer("missing-queue", None, Vec::new(), None)
        .await
        .expect("missing-queue fallback should start the queue app (announcement + hangup)");

    // Without return_to_ivr, the synthesized fallback plan starts the queue app
    // (which plays the service-unavailable announcement then hangs up).
    assert_eq!(
        runtime.started_apps(),
        vec!["queue".to_string()],
        "no return_to_ivr should start the queue app fallback, not IVR"
    );

    // A queue.not_found trace event must be recorded.
    let has_trace = session.meta.trace.iter().any(|ev| {
        ev.kind == crate::call_errors::TraceKind::Queue
            && ev.code.as_deref() == Some("queue.not_found")
    });
    assert!(
        has_trace,
        "expected a queue.not_found trace event; trace = {:?}",
        session.meta.trace
    );
}

// ─── voicemail: finalize recording on caller hangup ─────────────────────────
//
// Regression for the bug where a caller hanging up during voicemail recording
// lost the message: the BYE handler cancelled the app before `stop_recording`
// ran, so `on_record_complete` (the only thing that persists) never fired.
// We verify `finalize_recording_for_app_shutdown` finalizes an active
// recording (state → Idle, i.e. stop_recording executed) and is a no-op when
// nothing is recording.

#[tokio::test]
async fn finalize_recording_for_app_shutdown_finalizes_active_recording() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_application(
        "voicemail".to_string(),
        None,
        true,
    );
    let mut session = build_session(dialplan).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir
        .path()
        .join("voicemail.wav")
        .to_string_lossy()
        .into_owned();
    let mut bridge = playable_bridge("shutdown-recording").await;
    bridge
        .start_recording(path, 1, true, None)
        .await
        .expect("start recording");
    session.media.bridge = Some(bridge);

    session.finalize_recording_for_app_shutdown().await;

    assert!(
        std::fs::metadata(dir.path().join("voicemail.wav"))
            .expect("finalized recording")
            .len()
            >= 44,
        "active recording must be finalized on shutdown"
    );
}

#[tokio::test]
async fn finalize_recording_for_app_shutdown_noop_when_idle() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_application(
        "voicemail".to_string(),
        None,
        true,
    );
    let mut session = build_session(dialplan).await;
    session.media.bridge = Some(playable_bridge("shutdown-idle").await);
    // No file recorder is active; Stop is answered directly by the task.
    let start = Instant::now();
    session.finalize_recording_for_app_shutdown().await;
    let elapsed = start.elapsed();
    // No-op path must skip the finalization grace sleep.
    assert!(
        elapsed < std::time::Duration::from_millis(100),
        "idle path must be a fast no-op, took {:?}",
        elapsed
    );
}

#[tokio::test]
async fn queue_no_agents_hangup_fallback_starts_queue_app() {
    // Explicit Hangup fallback with a distinct failure code → queue app starts.
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_application(
        "ivr".to_string(),
        None,
        true,
    );
    let config = make_queue_config_with_fallback(
        "support",
        RouteQueueFallbackConfig {
            failure_code: Some(486),
            failure_reason: Some("All agents unavailable".to_string()),
            ..Default::default()
        },
    );
    let mut session = build_session_with_config(dialplan, config).await;

    let runtime = Arc::new(StartOnlyRuntime::new());
    session.app_runtime = runtime.clone();

    session
        .handle_queue_transfer("support", None, Vec::new(), None)
        .await
        .expect("queue app should start");
    assert_eq!(runtime.start_calls.load(Ordering::SeqCst), 1);
}

// ─── IVR → queue transfer: start_queue_app AlreadyRunning recovery ───────────
//
// Regression for the dead-air bug: when an IVR hands control to a queue via
// `AppAction::Transfer`, the IVR app is still registered as the running app on
// the runtime (only `stop_app` clears it). `start_queue_app` used to call
// `start_app` directly, so the queue app failed with `AlreadyRunning("queue")`
// and the caller sat in silence — no transfer/busy prompt, no fallback to
// `return_app`. It must recover exactly like `start_ivr_app` does: stop the
// stale app and restart.

#[tokio::test]
async fn queue_transfer_recovers_from_already_running() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_application(
        "ivr".to_string(),
        None,
        true,
    );
    let mut config = ProxyConfig::default();
    config.queues.insert(
        "db-1".to_string(),
        RouteQueueConfig {
            name: Some("to-agent".to_string()),
            strategy: RouteQueueStrategyConfig {
                targets: vec![RouteQueueTargetConfig {
                    uri: "skill-group:nonexistent".to_string(),
                    label: Some("no-agents".to_string()),
                }],
                ..Default::default()
            },
            ..RouteQueueConfig::default()
        },
    );
    let mut session = build_session_with_config(dialplan, config).await;

    let runtime = Arc::new(AlreadyRunningThenOkRuntime::new());
    session.app_runtime = runtime.clone();

    session
        .handle_queue_transfer("db-1", None, Vec::new(), None)
        .await
        .expect("queue transfer should recover from AlreadyRunning");

    assert_eq!(
        runtime.start_calls.load(Ordering::SeqCst),
        2,
        "queue app start should be retried after stopping the stale app"
    );
    assert_eq!(
        runtime.stop_calls.load(Ordering::SeqCst),
        1,
        "the stale running app should be stopped exactly once"
    );
}

/// Test runtime that fails every `queue` start (non-retryable `UnknownApp`)
/// but succeeds for other apps, recording which non-queue apps were started.
struct FailQueueStartRuntime {
    started_apps: std::sync::Mutex<Vec<String>>,
}

impl FailQueueStartRuntime {
    fn new() -> Self {
        Self {
            started_apps: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn started_apps(&self) -> Vec<String> {
        self.started_apps.lock().unwrap().clone()
    }
}

#[async_trait]
impl AppRuntime for FailQueueStartRuntime {
    async fn start_app(
        &self,
        app_name: &str,
        _params: Option<serde_json::Value>,
        _auto_answer: bool,
    ) -> crate::call::runtime::AppResult<()> {
        if app_name == "queue" {
            return Err(AppRuntimeError::UnknownApp(app_name.to_string()));
        }
        self.started_apps.lock().unwrap().push(app_name.to_string());
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

    fn current_app(&self) -> Option<String> {
        None
    }
}

#[tokio::test]
async fn queue_transfer_start_failure_with_return_app_returns_to_ivr() {
    // If the queue app cannot be started at all (beyond AlreadyRunning), a
    // configured `return_app` must still rescue the caller back into the app
    // instead of leaving dead air.
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_application(
        "ivr".to_string(),
        None,
        true,
    );
    let mut config = ProxyConfig::default();
    config.queues.insert(
        "db-1".to_string(),
        RouteQueueConfig {
            name: Some("to-agent".to_string()),
            strategy: RouteQueueStrategyConfig {
                targets: vec![RouteQueueTargetConfig {
                    uri: "skill-group:nonexistent".to_string(),
                    label: Some("no-agents".to_string()),
                }],
                ..Default::default()
            },
            ..RouteQueueConfig::default()
        },
    );
    let mut session = build_session_with_config(dialplan, config).await;

    let runtime = Arc::new(FailQueueStartRuntime::new());
    session.app_runtime = runtime.clone();

    session
        .handle_queue_transfer(
            "db-1",
            Some(crate::proxy::proxy_call::sip_session::ReturnTargetSpec {
                app_name: "ivr".to_string(),
                target: Some("asdf".to_string()),
                params: HashMap::new(),
            }),
            Vec::new(),
            None,
        )
        .await
        .expect("queue start failure with return_app should start the IVR app");

    assert_eq!(
        runtime.started_apps(),
        vec!["ivr".to_string()],
        "queue start failure should fall back to the return app, not start the queue"
    );

    // A queue.start_failed trace event must be recorded.
    let has_trace = session.meta.trace.iter().any(|ev| {
        ev.kind == crate::call_errors::TraceKind::Queue
            && ev.code.as_deref() == Some("queue.start_failed")
    });
    assert!(
        has_trace,
        "expected a queue.start_failed trace event; trace = {:?}",
        session.meta.trace
    );
}

// ── cc_ringing for queue-dialed agents (dynamic leg 180 Ringing) ─────────────

/// Recording hook that captures whether `on_call_ringing` fired.
struct RingingRecordingHook {
    ringing: Arc<AtomicUsize>,
}

impl RingingRecordingHook {
    fn new() -> (Self, Arc<AtomicUsize>) {
        let ringing = Arc::new(AtomicUsize::new(0));
        (
            Self {
                ringing: ringing.clone(),
            },
            ringing,
        )
    }
}

#[async_trait]
impl crate::proxy::proxy_call::session_hooks::CallSessionHook for RingingRecordingHook {
    async fn on_call_ringing(&self, _ctx: &CallSessionContext) {
        self.ringing.fetch_add(1, Ordering::SeqCst);
    }
}

/// Regression: a dynamic leg (queue-dialed agent) that receives 180 Ringing
/// must fire the `on_call_ringing` session hooks (which the CC addon turns into
/// `cc_ringing`). Before the fix, `initiate_sip_leg`'s spawned task only handled
/// 183 early media and never notified the session of a 180 Ringing, so
/// queue-dialed agents produced no `cc_ringing`.
#[tokio::test]
async fn test_leg_ringing_fires_on_call_ringing_hook() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto).with_queue(QueuePlan {
        queue_name: "support".to_string(),
        ..Default::default()
    });
    let (mut server, _config) = create_test_server().await;
    let (hook, ringing) = RingingRecordingHook::new();
    Arc::get_mut(&mut server)
        .expect("server must be uniquely owned for hook registration")
        .session_hooks = Arc::new(vec![Arc::new(hook)]);
    let mut session = build_session_on_server(server, dialplan).await;

    let agent_leg = LegId::from("queue-agent");
    session
        .legs
        .insert(agent_leg.clone(), Leg::new(agent_leg.clone()));

    session
        .execute_command(
            CallCommand::LegRinging {
                leg_id: agent_leg.clone(),
            },
            None,
        )
        .await;

    assert_eq!(
        ringing.load(Ordering::SeqCst),
        1,
        "on_call_ringing hook must fire when a dynamic leg rings"
    );
    assert_eq!(
        session.legs.get(&agent_leg).map(|l| l.state),
        Some(LegState::Ringing),
        "ringing leg should be marked LegState::Ringing"
    );
}

// ── hold-music resolution chain ──────────────────────────────────────────

fn expect_hold_music_file(resolved: Option<crate::call::domain::MediaSource>) -> String {
    match resolved {
        Some(crate::call::domain::MediaSource::File { path }) => path,
        other => panic!("expected File hold music, got: {other:?}"),
    }
}

/// The chain resolves: re-INVITE header > session extension > [proxy]
/// config > built-in default. Every agent hold (REST/console/supervisor)
/// goes through it, so the held party always hears hold audio.
#[tokio::test]
async fn test_resolve_hold_music_priority_chain() {
    // 4. Built-in default: no header, no extension, no config.
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto);
    let session = build_session(dialplan).await;
    assert_eq!(
        expect_hold_music_file(session.resolve_hold_music(&[])),
        crate::call::DEFAULT_QUEUE_HOLD_AUDIO,
        "empty chain must fall back to the built-in default hold audio"
    );

    // 3. [proxy].hold_music config.
    let mut config = ProxyConfig::default();
    config.hold_music = Some("sounds/from-config.wav".to_string());
    let session =
        build_session_with_config(build_dialplan_with_mode(MediaProxyMode::Auto), config).await;
    assert_eq!(
        expect_hold_music_file(session.resolve_hold_music(&[])),
        "sounds/from-config.wav"
    );

    // 2. Session extension (CC addon injects skill-group metadata.hold_music
    //    here as "X-Hold-Music").
    {
        let mut ext = session.extensions.write();
        ext.insert(HashMap::from([(
            "X-Hold-Music".to_string(),
            "sounds/from-extension.wav".to_string(),
        )]));
    }
    assert_eq!(
        expect_hold_music_file(session.resolve_hold_music(&[])),
        "sounds/from-extension.wav",
        "extension must win over the [proxy] config"
    );

    // 1. re-INVITE X-Hold-Music header beats everything.
    let headers = [rsipstack::sip::Header::Other(
        "X-Hold-Music".to_string(),
        "sounds/from-header.wav".to_string(),
    )];
    assert_eq!(
        expect_hold_music_file(session.resolve_hold_music(&headers)),
        "sounds/from-header.wav",
        "re-INVITE header must win over extension and config"
    );

    // http(s) values resolve to the Url variant.
    let url_headers = [rsipstack::sip::Header::Other(
        "X-Hold-Music".to_string(),
        "https://cdn.example.com/moh.mp3".to_string(),
    )];
    assert!(matches!(
        session.resolve_hold_music(&url_headers),
        Some(crate::call::domain::MediaSource::Url { .. })
    ));
}

// ── hangup command → immediate SIP BYE ──

/// Regression: `CallCommand::Hangup` must queue the affected dialogs into
/// `pending_hangup` so the session main loop sends the SIP BYE(s) on its
/// next iteration. Before the fix the command only marked legs `Ended` and
/// cancelled the token — the actual BYE sat behind the 3s shutdown drain
/// (or never went out because the remote hung up first), which was
/// user-visible as "the survey requested a hangup but the call stayed up".
#[tokio::test]
async fn hangup_command_queues_bye_dialogs_immediately() {
    use crate::call::domain::HangupCommand;

    let mut session = build_session(build_dialplan_with_mode(MediaProxyMode::Bypass)).await;
    let caller_dialog_id = session.caller_dialog_id();

    let result = session
        .execute_command(CallCommand::Hangup(HangupCommand::all(None, None)), None)
        .await;
    assert!(result.success, "hangup command must succeed");

    // The caller leg is ended AND its dialog is queued for an immediate BYE.
    assert_eq!(
        session.legs.get(&LegId::from("caller")).map(|l| l.state),
        Some(LegState::Ended)
    );
    assert!(
        session.pending_hangup.contains(&caller_dialog_id),
        "caller dialog must be queued in pending_hangup for an immediate BYE"
    );
}

/// `cascade = None` (single-leg semantics) must NOT queue any BYE — the
/// command is a no-op for dialogs in that mode.
#[tokio::test]
async fn hangup_command_cascade_none_queues_nothing() {
    use crate::call::domain::{HangupCascade, HangupCommand};

    let mut session = build_session(build_dialplan_with_mode(MediaProxyMode::Bypass)).await;

    let result = session
        .execute_command(
            CallCommand::Hangup(HangupCommand::all(None, None).with_cascade(HangupCascade::None)),
            None,
        )
        .await;
    assert!(result.success);
    assert!(
        session.pending_hangup.is_empty(),
        "cascade=None must not queue any BYE dialog"
    );
}

// ── queue-enricher INVITE header merge ──

#[test]
fn merge_leg_invite_headers_caller_headers_take_precedence() {
    use rsipstack::sip::Header;
    let caller = vec![
        Header::Other("User-to-User".into(), "root-1;purpose=call-center".into()),
        Header::Other(
            "Call-Info".into(),
            "<http://desk/cc>;purpose=call-center".into(),
        ),
    ];
    let location = vec![Header::Other(
        "Call-Info".into(),
        "<http://legacy/location>".into(),
    )];

    let merged =
        SipSession::merge_leg_invite_headers(caller.clone(), Some(location)).expect("merged");
    // Caller headers win duplicate-name resolution — the same-named location
    // header is DROPPED (dedup), not appended: the INVITE must carry each
    // header name once.
    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0].value(), caller[0].value());
    assert_eq!(merged[1].value(), caller[1].value());
}

/// Protocol-managed headers captured from a REGISTER (registrar stores the
/// full REGISTER header set on the location) must not leak into the new
/// leg's INVITE — the stack generates its own Contact / User-Agent / Via /
/// … and SIP.js REGISTERs also carry Allow / Supported / X-Auth-Token.
/// Regression for the Contact ×3 / User-Agent ×3 / X-Auth-Token ×2 INVITE.
#[test]
fn merge_leg_invite_headers_filters_protocol_headers_and_dedupes() {
    use rsipstack::sip::Header;
    let caller = vec![
        // Enricher/business headers — must survive.
        Header::Other("User-to-User".into(), "q=support;t=inbound".into()),
        Header::Other("X-Auth-Token".into(), "jwt-from-first-lookup".into()),
        // REGISTER-captured noise that arrived with the caller set.
        Header::Other(
            "Contact".into(),
            "<sip:abc@127.0.0.1:53847;transport=ws>;expires=300".into(),
        ),
        Header::Other("User-Agent".into(), "cc-phone/sip.js".into()),
    ];
    let location = vec![
        // Fresh locator lookup repeats the REGISTER set.
        Header::Other("X-Auth-Token".into(), "jwt-from-second-lookup".into()),
        Header::Other("Allow".into(), "ACK,CANCEL,INVITE".into()),
        Header::Other("Supported".into(), "outbound, path, gruu".into()),
        Header::Other("Expires".into(), "300".into()),
        Header::Other("Max-Forwards".into(), "70".into()),
        Header::Other(
            "Via".into(),
            "SIP/2.0/WS 127.0.0.1:53847;branch=z9hG4bKx".into(),
        ),
        Header::Other("From".into(), "<sip:bob@localhost>;tag=1".into()),
        Header::Other("Content-Type".into(), "application/sdp".into()),
    ];

    let merged = SipSession::merge_leg_invite_headers(caller, Some(location)).expect("merged");

    let count = |name: &str| {
        merged
            .iter()
            .filter(|h| h.name().eq_ignore_ascii_case(name))
            .count()
    };
    // Business headers kept exactly once (first occurrence wins).
    assert_eq!(count("User-to-User"), 1);
    assert_eq!(count("X-Auth-Token"), 1);
    assert_eq!(
        merged
            .iter()
            .find(|h| h.name().eq_ignore_ascii_case("X-Auth-Token"))
            .unwrap()
            .value(),
        "jwt-from-first-lookup"
    );
    // Protocol-managed names fully dropped from BOTH sets.
    for name in [
        "Contact",
        "User-Agent",
        "Allow",
        "Supported",
        "Expires",
        "Max-Forwards",
        "Via",
        "From",
        "Content-Type",
    ] {
        assert_eq!(count(name), 0, "header {name} must be filtered out");
    }
}

/// Without caller headers the location set is still filtered (the REGISTER
/// noise must not reach the INVITE even on the location-only path).
#[test]
fn merge_leg_invite_headers_filters_location_only_set() {
    use rsipstack::sip::Header;
    let location = vec![
        Header::Other("X-Route-Tag".into(), "edge-1".into()),
        Header::Other("Contact".into(), "<sip:abc@127.0.0.1>;expires=300".into()),
        Header::Other("User-Agent".into(), "cc-phone/sip.js".into()),
    ];
    let merged = SipSession::merge_leg_invite_headers(Vec::new(), Some(location)).unwrap();
    assert_eq!(merged.len(), 1);
    assert!(merged[0].name().eq_ignore_ascii_case("X-Route-Tag"));
}

#[test]
fn merge_leg_invite_headers_empty_caller_keeps_location_set() {
    use rsipstack::sip::Header;
    let location = vec![Header::Other("X-Loc".into(), "1".into())];
    let merged = SipSession::merge_leg_invite_headers(Vec::new(), Some(location.clone()));
    assert_eq!(merged, Some(location));

    // No caller headers and no location headers → still None.
    assert_eq!(SipSession::merge_leg_invite_headers(Vec::new(), None), None);
}

#[test]
fn merge_leg_invite_headers_no_location_headers() {
    use rsipstack::sip::Header;
    let caller = vec![Header::Other("X-Enrich".into(), "1".into())];
    let merged = SipSession::merge_leg_invite_headers(caller.clone(), None).expect("merged");
    assert_eq!(merged, caller);
}

// ── IVR start-failure fallback guards ──

fn route_point_config(action: crate::proxy::routing::RouteAction) -> ProxyConfig {
    use crate::proxy::routing::{MatchConditions, RouteRule};

    let mut config = ProxyConfig::default();
    config.routes = Some(vec![RouteRule {
        name: "route-point".to_string(),
        priority: 100,
        match_conditions: MatchConditions {
            request_uri_user: Some("39230".to_string()),
            ..Default::default()
        },
        action,
        ..Default::default()
    }]);
    config
}

fn route_point_dialplan() -> Dialplan {
    build_dialplan_with_mode(MediaProxyMode::Auto)
        .with_caller("sip:alice@rustpbx.test".try_into().unwrap())
}

async fn execute_route_point_transfer(
    session: &mut SipSession,
) -> crate::call::runtime::CommandResult {
    assert!(session.update_leg_state(&LegId::from("caller"), LegState::Connected));
    let (_callee_tx, mut callee_rx) = mpsc::unbounded_channel();
    session
        .execute_command(
            CallCommand::Transfer {
                leg_id: LegId::from("caller"),
                target: "toivr:39230".to_string(),
                attended: false,
            },
            Some(&mut callee_rx),
        )
        .await
}

fn assert_route_point_handoff_terminated(session: &SipSession) {
    assert_eq!(
        session
            .legs
            .get(&LegId::from("caller"))
            .map(|leg| leg.state),
        Some(LegState::Ended)
    );
    assert!(session.pending_hangup.contains(&session.caller_dialog_id()));
}

#[tokio::test]
async fn route_point_fallback_matches_current_invocation_context() {
    use crate::proxy::routing::MatchConditions;
    use sea_orm::DatabaseConnection;

    let mut config = ProxyConfig::default();
    config.ivr_fallback = Some(crate::config::IvrFallbackConfig {
        default: Some("original-safe".to_string()),
        rules: vec![crate::config::IvrFallbackRule {
            name: Some("routed-context".to_string()),
            priority: 100,
            match_conditions: MatchConditions {
                callee: Some("route-200".to_string()),
                headers: HashMap::from([("header.X-Business-Type".to_string(), "34".to_string())]),
                ..Default::default()
            },
            target: "routed-safe".to_string(),
        }],
    });
    let mut session = build_session_with_config(route_point_dialplan(), config).await;
    let app_context = Arc::new(ApplicationContext::new(
        DatabaseConnection::default(),
        CallInfo {
            session_id: "test-session".to_string(),
            caller: "alice".to_string(),
            callee: "original-100".to_string(),
            direction: "inbound".to_string(),
            started_at: chrono::Utc::now(),
            sip_headers: HashMap::from([("X-Business-Type".to_string(), "old".to_string())]),
            route_name: None,
        },
        Arc::new(crate::config::Config::default()),
    ));
    let mut runtime = RoutePointRuntime::new(&[]);
    runtime.context = Some(app_context);
    runtime.invocation = Some(AppInvocationContext {
        app_execution_id: 2,
        callee: "route-200".to_string(),
        sip_headers: HashMap::from([("X-Business-Type".to_string(), "34".to_string())]),
        variables: HashMap::new(),
    });
    let runtime = Arc::new(runtime);
    session.app_runtime = runtime.clone();

    session
        .try_ivr_fallback_after_start_failure(
            anyhow::anyhow!("route application failed"),
            "toivr:39230",
            &HashMap::new(),
        )
        .await
        .expect("current invocation should select a direct IVR fallback");

    let starts = runtime.started_apps();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].0, "ivr");
    assert!(
        starts[0]
            .1
            .as_ref()
            .and_then(|params| params.get("file"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|file| file.contains("routed-safe"))
    );
}

#[tokio::test]
async fn route_point_abort_terminates_without_fallback() {
    use crate::proxy::routing::RouteAction;

    let mut config = route_point_config(RouteAction {
        action: Some("busy".to_string()),
        ..Default::default()
    });
    config.ivr_fallback = Some(crate::config::IvrFallbackConfig {
        default: Some("safe-ivr".to_string()),
        rules: vec![],
    });
    let mut session = build_session_with_config(route_point_dialplan(), config).await;
    let runtime = Arc::new(RoutePointRuntime::new(&[]));
    session.app_runtime = runtime.clone();

    let result = execute_route_point_transfer(&mut session).await;

    assert!(!result.success);
    assert!(runtime.started_apps().is_empty());
    assert_route_point_handoff_terminated(&session);
}

#[tokio::test]
async fn route_point_miss_without_fallback_terminates() {
    let mut session =
        build_session_with_config(route_point_dialplan(), ProxyConfig::default()).await;
    session.app_runtime = Arc::new(RoutePointRuntime::new(&[]));

    let result = execute_route_point_transfer(&mut session).await;

    assert!(!result.success);
    assert_route_point_handoff_terminated(&session);
}

#[tokio::test]
async fn route_point_queue_result_starts_direct_ivr_fallback_once() {
    use crate::proxy::routing::RouteAction;

    let mut config = route_point_config(RouteAction {
        action: Some("queue".to_string()),
        queue: Some("support".to_string()),
        ..Default::default()
    });
    config.queues.insert(
        "support".to_string(),
        RouteQueueConfig {
            name: Some("support".to_string()),
            strategy: RouteQueueStrategyConfig {
                targets: vec![RouteQueueTargetConfig {
                    uri: "sip:agent@rustpbx.test".to_string(),
                    label: None,
                }],
                ..Default::default()
            },
            ..Default::default()
        },
    );
    config.ivr_fallback = Some(crate::config::IvrFallbackConfig {
        default: Some("safe-ivr".to_string()),
        rules: vec![],
    });
    let mut session = build_session_with_config(route_point_dialplan(), config).await;
    let runtime = Arc::new(RoutePointRuntime::new(&[]));
    session.app_runtime = runtime.clone();

    let result = execute_route_point_transfer(&mut session).await;

    assert!(result.success);
    assert_eq!(
        runtime
            .started_apps()
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
        vec!["ivr".to_string()]
    );
}

#[tokio::test]
async fn route_point_app_and_fallback_start_failure_terminates() {
    use crate::proxy::routing::RouteAction;

    let mut config = route_point_config(RouteAction {
        action: Some("application".to_string()),
        app: Some("step_ivr".to_string()),
        ..Default::default()
    });
    config.ivr_fallback = Some(crate::config::IvrFallbackConfig {
        default: Some("safe-ivr".to_string()),
        rules: vec![],
    });
    let mut session = build_session_with_config(route_point_dialplan(), config).await;
    let runtime = Arc::new(RoutePointRuntime::new(&["step_ivr", "ivr"]));
    session.app_runtime = runtime.clone();

    let result = execute_route_point_transfer(&mut session).await;

    assert!(!result.success);
    assert_eq!(
        runtime
            .started_apps()
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
        vec!["step_ivr".to_string(), "ivr".to_string()]
    );
    assert_route_point_handoff_terminated(&session);
}

/// With no `[proxy.ivr_fallback]` configured the original start error must
/// surface unchanged — no silent swallowing.
#[tokio::test]
async fn ivr_start_failure_without_fallback_config_returns_original_error() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto);
    let config = ProxyConfig::default();
    let session = build_session_with_config(dialplan, config).await;

    let err = session
        .try_ivr_fallback_after_start_failure(
            anyhow::anyhow!("IVR 'sales' failed to start"),
            "sales",
            &HashMap::new(),
        )
        .await
        .expect_err("unconfigured fallback must return the original error");
    assert!(
        err.to_string().contains("sales"),
        "original error must surface, got {err}"
    );
}

/// A retry flagged with `ivr_fallback_used=1` must not fall back again —
/// this guard is what prevents fallback loops between two broken IVRs.
#[tokio::test]
async fn ivr_start_failure_with_used_flag_does_not_fallback_again() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto);
    let mut config = ProxyConfig::default();
    config.ivr_fallback = Some(crate::config::IvrFallbackConfig {
        default: Some("fallback-ivr".to_string()),
        rules: vec![],
    });
    let session = build_session_with_config(dialplan, config).await;

    let mut query = HashMap::new();
    query.insert("ivr_fallback_used".to_string(), "1".to_string());
    let err = session
        .try_ivr_fallback_after_start_failure(
            anyhow::anyhow!("IVR 'fallback-ivr' failed again"),
            "fallback-ivr",
            &query,
        )
        .await
        .expect_err("used flag must short-circuit the fallback");
    assert!(err.to_string().contains("failed again"));
}

/// When the resolved fallback target is the very IVR that just failed,
/// retrying would loop forever — the original error must surface.
#[tokio::test]
async fn ivr_start_failure_with_same_target_does_not_retry() {
    let dialplan = build_dialplan_with_mode(MediaProxyMode::Auto);
    let mut config = ProxyConfig::default();
    config.ivr_fallback = Some(crate::config::IvrFallbackConfig {
        default: Some("broken-ivr".to_string()),
        rules: vec![],
    });
    let session = build_session_with_config(dialplan, config).await;

    let err = session
        .try_ivr_fallback_after_start_failure(
            anyhow::anyhow!("IVR 'broken-ivr' failed to start"),
            "broken-ivr",
            &HashMap::new(),
        )
        .await
        .expect_err("same-target fallback must not retry");
    assert!(err.to_string().contains("broken-ivr"));
}
