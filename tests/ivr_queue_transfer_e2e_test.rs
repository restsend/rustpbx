//! IVR → Queue → Transfer e2e (sipbot caller)
//!
//! Full chain with real RFC 4733 RTP DTMF and server-side B2BUA transfer:
//!   sipbot(caller) → IVR greeting → DTMF '1' → queue:support
//!     → skill-group:support → agent (sipbot echo)
//!     → RWI call.transfer → charlie (sipbot echo, fresh PBX INVITE)
//!
//! Uses the default `blind_transfer_use_refer = false` so the PBX performs a
//! server-side leg replacement (originates a new INVITE to the transfer
//! target) — the sipbot caller never needs to follow a REFER.
//!
//! Usage: cargo test --features addon-cc --test ivr_queue_transfer_e2e_test -- --nocapture

mod helpers;

use futures::{SinkExt, StreamExt};
use helpers::sipbot_helper::TestUa;
use helpers::test_server::{TEST_TOKEN, TestPbx, TestPbxInject};
use rustpbx::call::SipUser;
use rustpbx::call::app::agent_registry::{
    AgentRecord, AgentRegistry, PresenceState, RoutingStrategy,
};
use rustpbx::config::{ProxyConfig, UserBackendConfig};
use rustpbx::proxy::routing::{RouteQueueConfig, RouteRule};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use uuid::Uuid;

// ─── Agent registry (skill-group resolver) ────────────────────────────────

struct TestAgentRegistry {
    agents: RwLock<Vec<TestAgentEntry>>,
}

#[derive(Debug)]
struct TestAgentEntry {
    agent_id: String,
    display_name: String,
    uri: String,
    skills: Vec<String>,
    presence: PresenceState,
}

impl TestAgentRegistry {
    fn new() -> Self {
        Self {
            agents: RwLock::new(Vec::new()),
        }
    }

    async fn add_agent(&self, id: &str, name: &str, uri: &str, skills: Vec<&str>) {
        self.agents.write().await.push(TestAgentEntry {
            agent_id: id.to_string(),
            display_name: name.to_string(),
            uri: uri.to_string(),
            skills: skills.iter().map(|s| s.to_string()).collect(),
            presence: PresenceState::Idle,
        });
    }

    fn to_record(e: &TestAgentEntry) -> AgentRecord {
        AgentRecord {
            agent_id: e.agent_id.clone(),
            display_name: e.display_name.clone(),
            uri: e.uri.clone(),
            skills: e.skills.clone(),
            max_concurrency: 3,
            current_calls: 0,
            presence: e.presence.clone(),
            last_state_change: Instant::now(),
            total_calls_handled: 0,
            total_talk_time_secs: 0,
            last_call_end: None,
            custom_data: HashMap::new(),
        }
    }
}

#[async_trait::async_trait]
impl AgentRegistry for TestAgentRegistry {
    async fn register(
        &self,
        agent_id: String,
        display_name: String,
        uri: String,
        skills: Vec<String>,
        _max_concurrency: u32,
    ) -> anyhow::Result<()> {
        self.agents.write().await.push(TestAgentEntry {
            agent_id,
            display_name,
            uri,
            skills,
            presence: PresenceState::Idle,
        });
        Ok(())
    }

    async fn unregister(&self, agent_id: &str) -> anyhow::Result<()> {
        self.agents.write().await.retain(|a| a.agent_id != agent_id);
        Ok(())
    }

    async fn get_agent(&self, agent_id: &str) -> Option<AgentRecord> {
        self.agents
            .read()
            .await
            .iter()
            .find(|a| a.agent_id == agent_id)
            .map(Self::to_record)
    }

    async fn list_agents(&self) -> Vec<AgentRecord> {
        self.agents
            .read()
            .await
            .iter()
            .map(Self::to_record)
            .collect()
    }

    async fn update_presence(
        &self,
        agent_id: &str,
        new_state: PresenceState,
    ) -> anyhow::Result<()> {
        if let Some(a) = self
            .agents
            .write()
            .await
            .iter_mut()
            .find(|a| a.agent_id == agent_id)
        {
            a.presence = new_state;
        }
        Ok(())
    }

    async fn start_call(&self, agent_id: &str) -> anyhow::Result<()> {
        if let Some(a) = self
            .agents
            .write()
            .await
            .iter_mut()
            .find(|a| a.agent_id == agent_id)
        {
            a.presence = PresenceState::Busy { call_id: None };
        }
        Ok(())
    }

    async fn end_call(&self, agent_id: &str, _talk_time_secs: u64) -> anyhow::Result<()> {
        if let Some(a) = self
            .agents
            .write()
            .await
            .iter_mut()
            .find(|a| a.agent_id == agent_id)
        {
            a.presence = PresenceState::Wrapup { call_id: None };
        }
        Ok(())
    }

    async fn find_available_agents(&self, required_skills: &[String]) -> Vec<AgentRecord> {
        self.agents
            .read()
            .await
            .iter()
            .filter(|a| {
                matches!(a.presence, PresenceState::Idle)
                    && required_skills.iter().all(|s| a.skills.contains(s))
            })
            .map(Self::to_record)
            .collect()
    }

    async fn select_agent(
        &self,
        required_skills: &[String],
        _strategy: RoutingStrategy,
    ) -> Option<AgentRecord> {
        self.find_available_agents(required_skills)
            .await
            .into_iter()
            .next()
    }

    async fn resolve_target(&self, target_uri: &str) -> Vec<String> {
        if let Some(sg_id) = target_uri.strip_prefix("skill-group:") {
            let agents = self.agents.read().await;
            let matching: Vec<String> = agents
                .iter()
                .filter(|a| a.skills.iter().any(|s| s == sg_id))
                .map(|a| a.uri.clone())
                .collect();
            tracing::info!(
                "resolve_target '{}' → {} agents",
                target_uri,
                matching.len()
            );
            return matching;
        }
        vec![]
    }
}

// ─── Config builders ─────────────────────────────────────────────────────

fn build_ivr_config() -> String {
    r#"[ivr]
name = "support-test"
ivr_mode = "tree"

[ivr.root]
greeting = "config/sounds/hello_pcmu.wav"
timeout_ms = 10000
max_retries = 3

[[ivr.root.entries]]
key = "1"
action = { type = "queue", target = "support" }
"#
    .to_string()
}

fn build_queue_config() -> HashMap<String, RouteQueueConfig> {
    let toml_str = r#"
name = "support"
accept_immediately = true
passthrough_ringback = false

[strategy]
mode = "sequential"

[[strategy.targets]]
uri = "skill-group:support"
"#;
    let mut m = HashMap::new();
    m.insert(
        "support".to_string(),
        toml::from_str(toml_str).expect("queue config"),
    );
    m
}

fn build_routes(temp_dir: &std::path::Path) -> Vec<RouteRule> {
    let ivr_path = temp_dir.join("support-test-ivr.toml");
    std::fs::write(&ivr_path, build_ivr_config()).expect("write ivr");
    let s = format!(
        r#"
name = "support-test"
priority = 100
app = "ivr"
auto_answer = true

[match]
"to.user" = "support-test"

[app_params]
file = "{}"
"#,
        ivr_path.display()
    );
    vec![toml::from_str(&s).expect("route")]
}

// ─── RWI WebSocket helpers ───────────────────────────────────────────────

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn ws_connect(rwi_url: &str) -> WsStream {
    let url = format!("{}?token={}", rwi_url, TEST_TOKEN);
    let (ws, _) = timeout(Duration::from_secs(5), connect_async(&url))
        .await
        .expect("connect timeout")
        .expect("connect error");
    ws
}

async fn ws_send_recv_with_id(
    ws: &mut WsStream,
    action: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let action_id = Uuid::new_v4().to_string();
    let request = serde_json::json!({
        "rwi": "1.0",
        "action_id": action_id,
        "action": action,
        "params": params,
    });
    let json = serde_json::to_string(&request).unwrap();
    ws.send(Message::Text(json.into())).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline - tokio::time::Instant::now();
        let msg = timeout(remaining, ws.next())
            .await
            .expect("recv timeout waiting for action response")
            .expect("stream closed")
            .expect("ws error");
        if let Message::Text(t) = msg {
            let v: serde_json::Value = serde_json::from_str(&t).expect("invalid json");
            if v.get("action_id").and_then(|a| a.as_str()) == Some(&action_id) {
                return v;
            }
        }
    }
}

async fn wait_for_event(
    ws: &mut WsStream,
    event_type: &str,
    max_wait_secs: u64,
) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(max_wait_secs);
    loop {
        let remaining = deadline - tokio::time::Instant::now();
        let msg = timeout(remaining, ws.next())
            .await
            .unwrap_or_else(|_| panic!("timeout waiting for {}", event_type))
            .expect("stream closed")
            .expect("ws error");

        if let Message::Text(t) = msg {
            let json: serde_json::Value = serde_json::from_str(&t).expect("invalid json");
            if json.get(event_type).is_some() {
                return json;
            }
            if json["event_type"].as_str() == Some(event_type) {
                return serde_json::json!({ event_type: json });
            }
        }
    }
}

async fn wait_for_any_event(
    ws: &mut WsStream,
    event_types: &[&str],
    max_wait_secs: u64,
) -> (String, serde_json::Value) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(max_wait_secs);
    loop {
        let remaining = deadline - tokio::time::Instant::now();
        let msg = timeout(remaining, ws.next())
            .await
            .expect("timeout waiting for any expected event")
            .expect("stream closed")
            .expect("ws error");

        if let Message::Text(t) = msg {
            let json: serde_json::Value = serde_json::from_str(&t).expect("invalid json");
            for event_type in event_types {
                if json.get(*event_type).is_some() {
                    return ((*event_type).to_string(), json);
                }
                if json["event_type"].as_str() == Some(event_type) {
                    let mut wrapped = serde_json::Map::new();
                    wrapped.insert((*event_type).to_string(), json);
                    return (
                        (*event_type).to_string(),
                        serde_json::Value::Object(wrapped),
                    );
                }
            }
        }
    }
}

// ─── Test: IVR → Queue → Agent → Transfer ───────────────────────────────

#[tokio::test]
async fn test_ivr_queue_agent_transfer_flow() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("no SIP port");
    let caller_port = portpicker::pick_unused_port().expect("no caller port");
    let agent_port = portpicker::pick_unused_port().expect("no agent port");
    let charlie_port = portpicker::pick_unused_port().expect("no charlie port");

    let temp_dir = std::env::temp_dir().join(format!("rustpbx_ivr_xfer_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).expect("temp dir");

    // Agent registry: one agent with skill "support".
    let registry = Arc::new(TestAgentRegistry::new());
    let agent_uri = format!("sip:agent1@127.0.0.1:{}", agent_port);
    registry
        .add_agent("agent1", "Agent 1", &agent_uri, vec!["support"])
        .await;

    // Start PBX (default config: blind_transfer_use_refer = false → B2BUA).
    let proxy_config = ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(sip_port),
        ensure_user: Some(false),
        user_backends: vec![UserBackendConfig::Memory {
            users: Some(vec![SipUser {
                id: 0,
                enabled: true,
                username: "support-test".to_string(),
                password: None,
                realm: None,
                allow_guest_calls: true,
                ..Default::default()
            }]),
        }],
        ..Default::default()
    };
    let inject = TestPbxInject {
        proxy_config: Some(proxy_config),
        routes: Some(build_routes(&temp_dir)),
        queues: Some(build_queue_config()),
        agent_registry: Some(registry as Arc<dyn AgentRegistry>),
        ..Default::default()
    };
    let pbx = TestPbx::start_with_inject(sip_port, inject).await;
    tracing::info!("PBX up: sip={}, rwi={}", sip_port, pbx.rwi_url);

    // Agent: ring 2s, answer with echo.
    let agent = TestUa::callee_with_username(agent_port, 2, "agent1").await;
    // Transfer target: independent sipbot callee — the PBX originates a fresh
    // INVITE to it (server-side B2BUA transfer, no REFER follow needed).
    let charlie = TestUa::callee_with_username(charlie_port, 1, "charlie").await;

    // Caller: call support-test, send real RFC 4733 DTMF "1" after 2s.
    let target = format!("sip:support-test@127.0.0.1:{}", sip_port);
    let caller = TestUa::caller_with_dtmf(caller_port, "caller1", target.clone(), "2s:1").await;
    tracing::info!(
        "Caller up on {}, target={}",
        caller_port,
        target
    );

    // Phase 1: IVR → DTMF '1' → queue → agent answers → bidirectional RTP.
    tokio::time::sleep(Duration::from_secs(12)).await;

    assert!(
        agent.has_rtp_rx(),
        "Agent should have RX RTP. Stats: {}",
        agent.rtp_stats_summary()
    );
    assert!(
        caller.has_rtp_rx(),
        "Caller should have RX RTP. Stats: {}",
        caller.rtp_stats_summary()
    );
    let agent_quality = agent.audio_quality_summary();
    let caller_quality = caller.audio_quality_summary();
    assert!(
        agent_quality.has_audio(),
        "Agent should have non-silent audio (total={}, silence={})",
        agent_quality.total_frames,
        agent_quality.silence_frames
    );
    assert!(
        caller_quality.has_audio(),
        "Caller should have non-silent audio (total={}, silence={})",
        caller_quality.total_frames,
        caller_quality.silence_frames
    );
    println!(
        "[TEST] Phase 1 PASSED: IVR → DTMF '1' → queue → agent (bidirectional RTP); agent={} caller={}",
        agent.rtp_stats_summary(),
        caller.rtp_stats_summary()
    );

    // Connect RWI and find the caller's session id.
    let mut ws = ws_connect(&pbx.rwi_url).await;
    let resp = ws_send_recv_with_id(
        &mut ws,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    let resp = ws_send_recv_with_id(&mut ws, "session.list_calls", serde_json::json!({})).await;
    assert_eq!(resp["status"], "success", "list_calls failed: {:?}", resp);
    let calls = resp["data"].as_array().cloned().unwrap_or_default();
    println!(
        "[TEST] Active calls: {}",
        calls
            .iter()
            .map(|c| c["session_id"].as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let call_id = calls
        .iter()
        .find(|c| c["caller"].as_str().unwrap_or("").contains("caller1"))
        .or_else(|| calls.first())
        .and_then(|c| c["session_id"].as_str().map(|s| s.to_string()))
        .expect("no active call found for caller");
    println!("[TEST] Resolved call_id: {}", call_id);

    // Claim ownership of the call so transfer events are delivered to our WS
    // connection (RWI `send_event_to_call_owner` only reaches the owner).
    {
        let mut gw = pbx.gateway.write();
        let sessions = gw.get_all_sessions();
        let ws_session = sessions.last().cloned().expect("no RWI session");
        gw.claim_call_ownership(
            &ws_session,
            call_id.clone().into(),
            rustpbx::rwi::session::OwnershipMode::Control,
        )
        .expect("claim call ownership");
    }

    // Phase 2: transfer the call to charlie (B2BUA server-side).
    let charlie_dest = format!("sip:charlie@127.0.0.1:{}", charlie_port);
    let resp = ws_send_recv_with_id(
        &mut ws,
        "call.transfer",
        serde_json::json!({"call_id": call_id, "target": charlie_dest}),
    )
    .await;
    println!("[TEST] call.transfer response: {:?}", resp);
    assert_eq!(resp["status"], "success", "transfer failed: {:?}", resp);

    // Wait for the transfer to complete.
    let (event_type, event) = wait_for_any_event(
        &mut ws,
        &["call_transfer_accepted", "call_transferred", "call_transfer_failed"],
        10,
    )
    .await;
    println!("[TEST] Transfer event: {} {:?}", event_type, event);
    assert_ne!(
        event_type,
        "call_transfer_failed",
        "transfer failed: {:?}",
        event
    );

    // Charlie must receive real RTP from the PBX-originated INVITE (server-side
    // leg replacement bridged caller ↔ charlie).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    while tokio::time::Instant::now() < deadline {
        if charlie.has_rtp_rx() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert!(
        charlie.has_rtp_rx(),
        "Charlie should receive RTP after transfer. Stats: {}",
        charlie.rtp_stats_summary()
    );
    println!(
        "[TEST] Phase 2 PASSED: transfer to charlie (B2BUA), charlie RTP={}",
        charlie.rtp_stats_summary()
    );

    // Cleanup.
    caller.stop();
    agent.stop();
    charlie.stop();
    pbx.stop();
    let _ = std::fs::remove_dir_all(&temp_dir);
}
