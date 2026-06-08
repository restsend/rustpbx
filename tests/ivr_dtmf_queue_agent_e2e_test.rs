//! E2E Foundation Script: Real SIP flow tests
//!
//! Base infrastructure for testing the full call path:
//!   SIPBot(caller) → IVR → DTMF → Queue → SkillGroup → SIPBot(agent)
//!
//! Usage: cargo test --features addon-cc --test ivr_dtmf_queue_agent_e2e_test -- --nocapture

mod helpers;

use helpers::sipbot_helper::TestUa;
use helpers::test_server::TestPbx;
use rustpbx::call::app::agent_registry::{
    AgentRecord, AgentRegistry, PresenceState, RoutingStrategy,
};
use rustpbx::call::SipUser;
use rustpbx::config::{ProxyConfig, UserBackendConfig};
use rustpbx::proxy::routing::{RouteQueueConfig, RouteRule};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

// ─── Test Infrastructure ────────────────────────────────────────────────

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
            presence: PresenceState::Available,
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
            presence: PresenceState::Available,
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
        self.agents.read().await.iter().map(Self::to_record).collect()
    }

    async fn update_presence(
        &self,
        agent_id: &str,
        new_state: PresenceState,
    ) -> anyhow::Result<()> {
        if let Some(a) = self.agents.write().await.iter_mut().find(|a| a.agent_id == agent_id) {
            a.presence = new_state;
        }
        Ok(())
    }

    async fn start_call(&self, agent_id: &str) -> anyhow::Result<()> {
        if let Some(a) = self.agents.write().await.iter_mut().find(|a| a.agent_id == agent_id) {
            a.presence = PresenceState::Busy { call_id: None };
        }
        Ok(())
    }

    async fn end_call(&self, agent_id: &str, _talk_time_secs: u64) -> anyhow::Result<()> {
        if let Some(a) = self.agents.write().await.iter_mut().find(|a| a.agent_id == agent_id) {
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
                matches!(a.presence, PresenceState::Available)
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
        self.find_available_agents(required_skills).await.into_iter().next()
    }

    async fn resolve_target(&self, target_uri: &str) -> Vec<String> {
        if let Some(sg_id) = target_uri.strip_prefix("skill-group:") {
            let agents = self.agents.read().await;
            let matching: Vec<String> = agents
                .iter()
                .filter(|a| a.skills.iter().any(|s| s == sg_id))
                .map(|a| a.uri.clone())
                .collect();
            tracing::info!("resolve_target '{}' → {} agents", target_uri, matching.len());
            return matching;
        }
        vec![]
    }
}

// ─── Config Builders ────────────────────────────────────────────────────

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
"#.to_string()
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
    m.insert("support".to_string(), toml::from_str(toml_str).expect("queue config"));
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

// ─── Tests ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_ivr_dtmf_queue_agent_flow() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("no SIP port");
    let agent_port = portpicker::pick_unused_port().expect("no agent port");
    let caller_port = portpicker::pick_unused_port().expect("no caller port");

    let temp_dir = std::env::temp_dir().join(format!("rustpbx_e2e_{}", std::process::id()));
    std::fs::create_dir_all(&temp_dir).expect("temp dir");

    // Agent registry: one agent with skill "support"
    let registry = Arc::new(TestAgentRegistry::new());
    let agent_uri = format!("sip:agent1@127.0.0.1:{}", agent_port);
    registry.add_agent("agent1", "Agent 1", &agent_uri, vec!["support"]).await;

    // Start PBX
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
    let inject = helpers::test_server::TestPbxInject {
        proxy_config: Some(proxy_config),
        routes: Some(build_routes(&temp_dir)),
        queues: Some(build_queue_config()),
        agent_registry: Some(registry as Arc<dyn AgentRegistry>),
        ..Default::default()
    };
    let pbx = TestPbx::start_with_inject(sip_port, inject).await;
    tracing::info!("PBX up: sip={}, rwi={}", sip_port, pbx.rwi_url);

    // Agent: listen, ring 2s, answer with echo
    let agent = TestUa::callee_with_username(agent_port, 2, "agent1").await;
    tracing::info!("Agent up on {}", agent_port);

    // Caller: call support-test, send DTMF "1" after 2s, hangup after 20s
    let target = format!("sip:support-test@127.0.0.1:{}", sip_port);
    let caller = TestUa::caller_with_dtmf(
        caller_port,
        "caller1",
        target.clone(),
        "2s:1",
    )
    .await;
    tracing::info!("Caller up on {}, target={}", caller_port, target);

    // Wait for: IVR answer → greeting → DTMF "1" → queue → agent ring → answer → audio
    tokio::time::sleep(std::time::Duration::from_secs(12)).await;

    let caller_rx = caller.rtp_stats_summary();
    let caller_quality = caller.audio_quality_summary();
    tracing::info!("Caller RTP: {}, quality: total={} silence={}", caller_rx, caller_quality.total_frames, caller_quality.silence_frames);

    let agent_rx = agent.rtp_stats_summary();
    let agent_quality = agent.audio_quality_summary();
    tracing::info!("Agent RTP: {}, quality: total={} silence={}", agent_rx, agent_quality.total_frames, agent_quality.silence_frames);

    assert!(agent.has_rtp_rx(), "Agent should have RX RTP. Stats: {}", agent_rx);
    assert!(caller.has_rtp_rx(), "Caller should have RX RTP. Stats: {}", caller_rx);
    assert!(agent_quality.has_audio(), "Agent should have non-silent audio (total={}, silence={})", agent_quality.total_frames, agent_quality.silence_frames);
    assert!(caller_quality.has_audio(), "Caller should have non-silent audio (total={}, silence={})", caller_quality.total_frames, caller_quality.silence_frames);

    tracing::info!("=== IVR → DTMF → Queue → Agent PASSED ===");

    caller.stop();
    agent.stop();
    pbx.stop();
    let _ = std::fs::remove_dir_all(&temp_dir);
}
