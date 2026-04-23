mod helpers;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use helpers::sipbot_helper::TestUa;
use helpers::test_server::{TEST_TOKEN, TestPbx, TestPbxInject};
use rustpbx::config::{MediaProxyMode, ProxyConfig};
use rustpbx::call::app::agent_registry::{AgentRecord, AgentRegistry, MemoryRegistry, PresenceState, RoutingStrategy};
use rustpbx::proxy::routing::{MatchConditions, RouteAction, RouteQueueConfig, RouteQueueStrategyConfig, RouteQueueTargetConfig, RouteRule};
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use uuid::Uuid;

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct SkillGroupRegistry {
    inner: MemoryRegistry,
}

impl SkillGroupRegistry {
    fn new() -> Self {
        Self {
            inner: MemoryRegistry::new(),
        }
    }
}

#[async_trait::async_trait]
impl AgentRegistry for SkillGroupRegistry {
    async fn register(
        &self,
        agent_id: String,
        display_name: String,
        uri: String,
        skills: Vec<String>,
        max_concurrency: u32,
    ) -> anyhow::Result<()> {
        self.inner
            .register(agent_id, display_name, uri, skills, max_concurrency)
            .await
    }

    async fn unregister(&self, agent_id: &str) -> anyhow::Result<()> {
        self.inner.unregister(agent_id).await
    }

    async fn get_agent(&self, agent_id: &str) -> Option<AgentRecord> {
        self.inner.get_agent(agent_id).await
    }

    async fn list_agents(&self) -> Vec<AgentRecord> {
        self.inner.list_agents().await
    }

    async fn update_presence(
        &self,
        agent_id: &str,
        new_state: PresenceState,
    ) -> anyhow::Result<()> {
        self.inner.update_presence(agent_id, new_state).await
    }

    async fn start_call(&self, agent_id: &str) -> anyhow::Result<()> {
        self.inner.start_call(agent_id).await
    }

    async fn end_call(&self, agent_id: &str, talk_time_secs: u64) -> anyhow::Result<()> {
        self.inner.end_call(agent_id, talk_time_secs).await
    }

    async fn find_available_agents(&self, required_skills: &[String]) -> Vec<AgentRecord> {
        self.inner.find_available_agents(required_skills).await
    }

    async fn select_agent(
        &self,
        required_skills: &[String],
        strategy: RoutingStrategy,
    ) -> Option<AgentRecord> {
        self.inner.select_agent(required_skills, strategy).await
    }

    async fn resolve_target(&self, target_uri: &str) -> Vec<String> {
        if let Some(skill_group) = target_uri.strip_prefix("skill-group:") {
            let key = skill_group.trim();
            if key.is_empty() {
                return vec![];
            }
            return self
                .inner
                .list_agents()
                .await
                .into_iter()
                .filter(|a| a.has_capacity() && a.skills.iter().any(|s| s == key))
                .map(|a| a.uri)
                .collect();
        }
        vec![]
    }

    async fn on_state_change(&self, handler: Box<dyn Fn(&AgentRecord) + Send + Sync>) {
        self.inner.on_state_change(handler).await
    }
}

async fn ws_connect(rwi_url: &str) -> WsStream {
    let url = format!("{}?token={}", rwi_url, TEST_TOKEN);
    let (ws, _) = timeout(Duration::from_secs(5), connect_async(&url))
        .await
        .expect("connect timeout")
        .expect("connect error");
    ws
}

fn req(action: &str, params: serde_json::Value) -> (String, String) {
    let id = Uuid::new_v4().to_string();
    let json = serde_json::to_string(&serde_json::json!({
        "rwi": "1.0",
        "action_id": id,
        "action": action,
        "params": params,
    }))
    .unwrap();
    (id, json)
}

async fn ws_send_recv_with_id(ws: &mut WsStream, json: &str, action_id: &str) -> serde_json::Value {
    ws.send(Message::Text(json.into())).await.unwrap();
    loop {
        let msg = timeout(Duration::from_secs(15), ws.next())
            .await
            .expect("recv timeout")
            .expect("stream ended")
            .expect("ws error");
        match msg {
            Message::Text(t) => {
                let v: serde_json::Value = serde_json::from_str(&t).expect("not JSON");
                if v.get("action_id").map(|a| a == action_id).unwrap_or(false) {
                    return v;
                }
            }
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected frame: {other:?}"),
        }
    }
}

async fn recv_until(
    ws: &mut WsStream,
    timeout_secs: u64,
    predicate: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("recv_until timeout");
        }
        let msg = timeout(remaining, ws.next())
            .await
            .expect("recv timeout")
            .expect("stream ended")
            .expect("ws error");
        let v: serde_json::Value = match msg {
            Message::Text(t) => serde_json::from_str(&t).expect("not JSON"),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected frame: {other:?}"),
        };
        if predicate(&v) {
            return v;
        }
    }
}

fn build_cc_routes() -> (Vec<RouteRule>, HashMap<String, RouteQueueConfig>) {
    let ivr = RouteRule {
        name: "cc_ivr_entry".to_string(),
        priority: 5,
        match_conditions: MatchConditions {
            to_user: Some("ivrcc".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            app: Some("ivr".to_string()),
            app_params: Some(serde_json::json!({
                "file": "/tmp/cc_full_flow_ivr.toml"
            })),
            auto_answer: false,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut queues = HashMap::new();
    queues.insert(
        "support".to_string(),
        RouteQueueConfig {
            name: Some("support".to_string()),
            strategy: RouteQueueStrategyConfig {
                targets: vec![RouteQueueTargetConfig {
                    uri: "skill-group:support".to_string(),
                    label: Some("Support SG".to_string()),
                }],
                ..Default::default()
            },
            accept_immediately: false,
            ..Default::default()
        },
    );

    (vec![ivr], queues)
}

fn write_test_ivr(path: &str) {
    let toml = r#"
[ivr]
name = "cc-flow"
lang = "en"

[ivr.root]
greeting = ""
timeout_ms = 800
max_retries = 1
timeout_action = { type = "queue", target = "support" }

[[ivr.root.entries]]
key = "2"
label = "Support"
action = { type = "queue", target = "support" }
"#;
    std::fs::write(path, toml).expect("write ivr file");
}

async fn assert_rtp_flow(
    caller: &TestUa,
    callee: &TestUa,
    label: &str,
    timeout_secs: u64,
) {
    let started = tokio::time::Instant::now();
    loop {
        if caller.has_rtp_tx() && caller.has_rtp_rx() && callee.has_rtp_tx() && callee.has_rtp_rx() {
            break;
        }
        if started.elapsed() > Duration::from_secs(timeout_secs) {
            panic!(
                "RTP not flowing for {label}. caller={}, callee={}",
                caller.rtp_stats_summary(),
                callee.rtp_stats_summary()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn assert_rtp_rx(ua: &TestUa, label: &str, timeout_secs: u64) {
    let started = tokio::time::Instant::now();
    loop {
        if ua.has_rtp_rx() {
            break;
        }
        if started.elapsed() > Duration::from_secs(timeout_secs) {
            panic!("RTP RX not flowing for {label}. stats={}", ua.rtp_stats_summary());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn setup_cc_env(register_support_agent: bool) -> (TestPbx, TestUa, TestUa, TestUa, WsStream, u16, u16, u16) {
    let sip_port = portpicker::pick_unused_port().expect("sip");
    let agent_port = portpicker::pick_unused_port().expect("agent");
    let transfer_port = portpicker::pick_unused_port().expect("transfer");
    let supervisor_port = portpicker::pick_unused_port().expect("supervisor");

    write_test_ivr("/tmp/cc_full_flow_ivr.toml");

    let (routes, queues) = build_cc_routes();
    let registry = Arc::new(SkillGroupRegistry::new());
    let mut proxy_config = ProxyConfig::default();
    proxy_config.modules = Some(vec!["call".to_string()]);
    proxy_config.media_proxy = MediaProxyMode::All;
    proxy_config.enable_latching = true;

    if register_support_agent {
        registry
            .register(
                "agent-001".to_string(),
                "Support Agent".to_string(),
                format!("sip:agent@127.0.0.1:{}", agent_port),
                vec!["support".to_string()],
                1,
            )
            .await
            .unwrap();
    }

    let pbx = TestPbx::start_with_inject(
        sip_port,
        TestPbxInject {
            proxy_config: Some(proxy_config),
            routes: Some(routes),
            queues: Some(queues),
            agent_registry: Some(registry),
            ..Default::default()
        },
    )
    .await;

    let agent = TestUa::callee_with_username(agent_port, 1, "agent").await;
    let transfer_target = TestUa::callee_with_username(transfer_port, 1, "charlie").await;
    let supervisor = TestUa::callee_with_username(supervisor_port, 1, "supervisor").await;

    let mut ws = ws_connect(&pbx.rwi_url).await;
    let (sub_id, sub_json) = req("session.subscribe", serde_json::json!({"contexts": ["default"]}));
    let sub = ws_send_recv_with_id(&mut ws, &sub_json, &sub_id).await;
    assert_eq!(sub["status"], "success");

    (pbx, agent, transfer_target, supervisor, ws, sip_port, transfer_port, supervisor_port)
}

async fn originate_via_ivr_and_wait_answer(ws: &mut WsStream, sip_port: u16) -> String {
    let call_id = format!("cc-flow-{}", Uuid::new_v4());
    let (orig_id, orig_json) = req(
        "call.originate",
        serde_json::json!({
            "call_id": call_id,
            "destination": format!("sip:ivrcc@127.0.0.1:{}", sip_port),
            "caller_id": format!("sip:caller@127.0.0.1:{}", sip_port),
            "timeout_secs": 20,
        }),
    );
    let orig = ws_send_recv_with_id(ws, &orig_json, &orig_id).await;
    assert_eq!(orig["status"], "success", "originate failed: {orig}");

    let answered = recv_until(ws, 20, |v| {
        v.get("call_answered").is_some() && v["call_answered"]["call_id"] == call_id
    })
    .await;
    assert_eq!(answered["call_answered"]["call_id"], call_id);
    call_id
}

async fn wait_for_active_call_id(ws: &mut WsStream, timeout_secs: u64) -> String {
    let started = tokio::time::Instant::now();
    loop {
        let (list_id, list_json) = req("session.list_calls", serde_json::json!({}));
        let listed = ws_send_recv_with_id(ws, &list_json, &list_id).await;
        if listed["status"] == "success"
            && let Some(arr) = listed["data"].as_array()
                && let Some(call) = arr.first()
                    && let Some(call_id) = call["session_id"].as_str() {
                        return call_id.to_string();
                    }
        if started.elapsed() > Duration::from_secs(timeout_secs) {
            panic!("No active call found via session.list_calls: {listed}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_call_status(ws: &mut WsStream, call_id: &str, status: &str, timeout_secs: u64) {
    let started = tokio::time::Instant::now();
    loop {
        let (list_id, list_json) = req("session.list_calls", serde_json::json!({}));
        let listed = ws_send_recv_with_id(ws, &list_json, &list_id).await;
        if listed["status"] == "success"
            && let Some(arr) = listed["data"].as_array()
                && arr.iter().any(|call| {
                    call["session_id"] == call_id && call["status"] == status
                }) {
                    return;
                }

        if started.elapsed() > Duration::from_secs(timeout_secs) {
            panic!(
                "Call {call_id} did not reach status {status}. Last session.list_calls={listed}"
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn originate_supervisor_and_wait_answer(ws: &mut WsStream, sip_port: u16, supervisor_port: u16) -> String {
    let supervisor_call_id = format!("cc-supervisor-{}", Uuid::new_v4());
    let (sup_orig_id, sup_orig_json) = req(
        "call.originate",
        serde_json::json!({
            "call_id": supervisor_call_id,
            "destination": format!("sip:supervisor@127.0.0.1:{}", supervisor_port),
            "caller_id": format!("sip:supervisor@127.0.0.1:{}", sip_port),
            "timeout_secs": 15,
        }),
    );
    let sup_orig = ws_send_recv_with_id(ws, &sup_orig_json, &sup_orig_id).await;
    assert_eq!(sup_orig["status"], "success", "supervisor originate failed: {sup_orig}");

    let _sup_answered = recv_until(ws, 20, |v| {
        v.get("call_answered").is_some() && v["call_answered"]["call_id"] == supervisor_call_id
    })
    .await;
    supervisor_call_id
}

#[tokio::test]
async fn test_cc_flow_ivr_queue_skillgroup_agent_refer_supervisor_listen() {
    let _ = tracing_subscriber::fmt::try_init();

    let (pbx, agent, transfer_target, supervisor, mut ws, sip_port, transfer_port, supervisor_port) =
        setup_cc_env(true).await;

    let caller_port = portpicker::pick_unused_port().expect("caller");
    let caller = TestUa::caller_with_target(
        caller_port,
        "caller",
        format!("sip:ivrcc@127.0.0.1:{}", sip_port),
    )
    .await;

    let call_id = wait_for_active_call_id(&mut ws, 20).await;

    let (answer_id, answer_json) = req(
        "call.answer",
        serde_json::json!({
            "call_id": call_id,
        }),
    );
    let answer = ws_send_recv_with_id(&mut ws, &answer_json, &answer_id).await;
    assert_eq!(answer["status"], "success", "call answer failed: {answer}");
    wait_for_call_status(&mut ws, &call_id, "talking", 20).await;

    // Let IVR timeout into queue and connect the support agent before REFER.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_rtp_flow(&caller, &agent, "caller-agent media before REFER", 20).await;

    let transfer_uri = format!("sip:charlie@127.0.0.1:{}", transfer_port);

    let (xfer_id, xfer_json) = req(
        "call.transfer",
        serde_json::json!({
            "call_id": call_id,
            "target": transfer_uri,
        }),
    );
    let xfer = ws_send_recv_with_id(&mut ws, &xfer_json, &xfer_id).await;
    assert_eq!(xfer["status"], "success", "transfer failed: {xfer}");

    let supervisor_call_id =
        originate_supervisor_and_wait_answer(&mut ws, sip_port, supervisor_port).await;

    let (listen_id, listen_json) = req(
        "supervisor.listen",
        serde_json::json!({
            "supervisor_call_id": supervisor_call_id,
            "target_call_id": call_id,
        }),
    );
    let listen = ws_send_recv_with_id(&mut ws, &listen_json, &listen_id).await;
    assert_eq!(listen["status"], "success", "supervisor listen failed: {listen}");

    assert_rtp_rx(&supervisor, "supervisor media path after listen", 20).await;

    ws.close(None).await.unwrap();
    caller.stop();
    agent.stop();
    transfer_target.stop();
    supervisor.stop();
    pbx.stop();
}

#[tokio::test]
async fn test_cc_flow_supervisor_whisper_and_barge() {
    let _ = tracing_subscriber::fmt::try_init();

    let (pbx, agent, transfer_target, supervisor, mut ws, sip_port, _transfer_port, supervisor_port) =
        setup_cc_env(true).await;

    let call_id = originate_via_ivr_and_wait_answer(&mut ws, sip_port).await;
    let supervisor_call_id =
        originate_supervisor_and_wait_answer(&mut ws, sip_port, supervisor_port).await;

    let (whisper_id, whisper_json) = req(
        "supervisor.whisper",
        serde_json::json!({
            "supervisor_call_id": supervisor_call_id,
            "target_call_id": call_id,
            "agent_leg": "",
        }),
    );
    let whisper = ws_send_recv_with_id(&mut ws, &whisper_json, &whisper_id).await;
    assert_eq!(whisper["status"], "success", "supervisor whisper failed: {whisper}");

    let (barge_id, barge_json) = req(
        "supervisor.barge",
        serde_json::json!({
            "supervisor_call_id": supervisor_call_id,
            "target_call_id": call_id,
            "agent_leg": "",
        }),
    );
    let barge = ws_send_recv_with_id(&mut ws, &barge_json, &barge_id).await;
    assert_eq!(barge["status"], "success", "supervisor barge failed: {barge}");

    ws.close(None).await.unwrap();
    agent.stop();
    transfer_target.stop();
    supervisor.stop();
    pbx.stop();
}

#[tokio::test]
async fn test_cc_flow_queue_fallback_when_no_skill_group_agents() {
    let _ = tracing_subscriber::fmt::try_init();

    let (pbx, agent, transfer_target, supervisor, mut ws, sip_port, _transfer_port, _supervisor_port) =
        setup_cc_env(false).await;

    let call_id = format!("cc-flow-fallback-{}", Uuid::new_v4());
    let (orig_id, orig_json) = req(
        "call.originate",
        serde_json::json!({
            "call_id": call_id,
            "destination": format!("sip:ivrcc@127.0.0.1:{}", sip_port),
            "caller_id": format!("sip:caller@127.0.0.1:{}", sip_port),
            "timeout_secs": 20,
        }),
    );
    let orig = ws_send_recv_with_id(&mut ws, &orig_json, &orig_id).await;
    assert_eq!(orig["status"], "success", "originate failed: {orig}");

    let fallback_call_id = format!("cc-flow-fallback-check-{}", Uuid::new_v4());
    let (fallback_id, fallback_json) = req(
        "call.originate",
        serde_json::json!({
            "call_id": fallback_call_id,
            "destination": format!("sip:support@127.0.0.1:{}", sip_port),
            "caller_id": format!("sip:caller@127.0.0.1:{}", sip_port),
            "timeout_secs": 8,
        }),
    );
    let fallback_orig = ws_send_recv_with_id(&mut ws, &fallback_json, &fallback_id).await;
    assert_eq!(fallback_orig["status"], "success", "fallback originate failed: {fallback_orig}");

    let end_event = recv_until(&mut ws, 20, |v| {
        (v.get("call_hangup").is_some() && v["call_hangup"]["call_id"] == call_id)
            || (v.get("call_no_answer").is_some() && v["call_no_answer"]["call_id"] == call_id)
            || (v.get("call_hangup").is_some() && v["call_hangup"]["call_id"] == fallback_call_id)
            || (v.get("call_no_answer").is_some() && v["call_no_answer"]["call_id"] == fallback_call_id)
    })
    .await;

    assert!(
        end_event.get("call_hangup").is_some() || end_event.get("call_no_answer").is_some(),
        "expected fallback end event, got: {end_event}"
    );

    ws.close(None).await.unwrap();
    agent.stop();
    transfer_target.stop();
    supervisor.stop();
    pbx.stop();
}
