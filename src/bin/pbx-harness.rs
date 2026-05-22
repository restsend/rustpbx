//! PBX Test Harness — starts TestPbx with IVR+Queue+Agent routing
//! and real CC REST API backed by SQLite database.

use std::collections::HashMap;
use std::sync::Arc;

use portpicker::pick_unused_port;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use rustpbx::addons::cc::tests::helpers::{
    cc_api_router,
    sipbot_control::{SipControlState, sipbot_control_router},
    test_server::{TestPbx, TestPbxInject},
};
use rustpbx::call::app::agent_registry::{
    AgentRecord, AgentRegistry, MemoryRegistry, PresenceState, RoutingStrategy,
};
use rustpbx::config::ProxyConfig;
use rustpbx::proxy::routing::{
    MatchConditions, RouteAction, RouteQueueConfig, RouteQueueStrategyConfig,
    RouteQueueTargetConfig, RouteRule,
};

#[derive(Serialize)]
struct HarnessInfo {
    sip_port: u16,
    ws_url: String,
    rwi_url: String,
    http_port: u16,
    pid: u32,
    ivr_entry: String,
    db_type: String,
}

// ── Custom agent registry with skill-group resolve support ──────────────────
struct HarnessRegistry {
    inner: MemoryRegistry,
}

#[async_trait::async_trait]
impl AgentRegistry for HarnessRegistry {
    async fn register(&self, agent_id: String, display_name: String, uri: String,
        skills: Vec<String>, max_concurrency: u32) -> anyhow::Result<()> {
        self.inner.register(agent_id, display_name, uri, skills, max_concurrency).await
    }
    async fn unregister(&self, agent_id: &str) -> anyhow::Result<()> { self.inner.unregister(agent_id).await }
    async fn get_agent(&self, agent_id: &str) -> Option<AgentRecord> { self.inner.get_agent(agent_id).await }
    async fn list_agents(&self) -> Vec<AgentRecord> { self.inner.list_agents().await }
    async fn update_presence(&self, agent_id: &str, new_state: PresenceState) -> anyhow::Result<()> {
        self.inner.update_presence(agent_id, new_state).await
    }
    async fn start_call(&self, agent_id: &str) -> anyhow::Result<()> { self.inner.start_call(agent_id).await }
    async fn end_call(&self, agent_id: &str, talk_time_secs: u64) -> anyhow::Result<()> {
        self.inner.end_call(agent_id, talk_time_secs).await
    }
    async fn find_available_agents(&self, required_skills: &[String]) -> Vec<AgentRecord> {
        self.inner.find_available_agents(required_skills).await
    }
    async fn select_agent(&self, required_skills: &[String], strategy: RoutingStrategy) -> Option<AgentRecord> {
        self.inner.select_agent(required_skills, strategy).await
    }
    async fn resolve_target(&self, target_uri: &str) -> Vec<String> {
        if let Some(skill_group) = target_uri.strip_prefix("skill-group:") {
            let key = skill_group.trim();
            if key.is_empty() { return vec![]; }
            let agents = self.list_agents().await;
            return agents.iter()
                .filter(|a| a.presence == PresenceState::Available
                    && a.skills.iter().any(|s| s == key))
                .map(|a| a.uri.clone())
                .collect();
        }
        vec![]
    }
}

impl HarnessRegistry {
    fn new() -> Self { Self { inner: MemoryRegistry::new() } }
}

// ── Main ───────────────────────────────────────────────────────────────────
#[tokio::main]
async fn main() {
    let cancel_token = CancellationToken::new();
    let sip_port = pick_unused_port().expect("no free UDP port");

    // ── Write IVR config ────────────────────────────────────────────────────
    let ivr_path = "/tmp/cc_pbx_harness_ivr.toml";
    std::fs::write(ivr_path, r#"
[ivr]
name = "cc-harness"
lang = "en"
[ivr.root]
greeting = ""
timeout_ms = 500
max_retries = 1
timeout_action = { type = "queue", target = "support" }
[[ivr.root.entries]]
key = "1"
label = "Support"
action = { type = "queue", target = "support" }
"#).ok();

    // ── Route rules (ivrcc → IVR → queue "support") ────────────────────────
    let ivr_route = RouteRule {
        match_conditions: MatchConditions {
            to_user: Some("ivrcc".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            app: Some("ivr".to_string()),
            app_params: Some(serde_json::json!({"file": ivr_path})),
            auto_answer: false,
            ..Default::default()
        },
        ..Default::default()
    };

    // ── Queue config ────────────────────────────────────────────────────────
    let mut queues = HashMap::new();
    queues.insert("support".to_string(), RouteQueueConfig {
        strategy: RouteQueueStrategyConfig {
            wait_timeout_secs: Some(60),
            targets: vec![RouteQueueTargetConfig {
                uri: "skill-group:support".to_string(),
                label: Some("Support SG".to_string()),
            }],
            ..Default::default()
        },
        ..Default::default()
    });

    // ── Agent registry with skill-group resolve ─────────────────────────────
    let registry: Arc<dyn AgentRegistry> = Arc::new(HarnessRegistry::new());
    registry.register(
        "agent-1001".into(), "Agent 1001".into(),
        "sip:agent-1001@127.0.0.1".into(),
        vec!["support".into()], 1,
    ).await.unwrap();

    // ── Create SQLite database + seed agent-1001 ────────────────────────────
    let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
    use sea_orm_migration::MigratorTrait;
    rustpbx::addons::cc::migration::Migrator::up(&db, None).await.unwrap();

    // Register agent-1001 in the database
    use sea_orm::{ActiveModelTrait, Set};
    let now = chrono::Utc::now();
    use rustpbx::addons::cc::models::cc_agent::ActiveModel as AgentActiveModel;
    let agent_model = AgentActiveModel {
        agent_id: Set("agent-1001".to_string()),
        display_name: Set(Some("Agent 1001".to_string())),
        primary_endpoint: Set(Some("agent-1001".to_string())),
        skills: Set(serde_json::json!(["support"])),
        max_concurrency: Set(1),
        role: Set(Some("agent".to_string())),
        is_active: Set(true),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    };
    agent_model.insert(&db).await.unwrap();

    // ── Create CcAddonState with the real DB and in-memory agent registry ───
    let cc_state = Arc::new(
        rustpbx::addons::cc::CcAddonState::with_db(db.clone())
    );
    // Load agents from DB into AgentRegistry
    cc_state.agent_registry.load_from_db().await.unwrap();

    // ── CC API router (production-style, real data) ─────────────────────────
    let cc_router = cc_api_router::cc_api_router(cc_state.clone());

    // ── Sipbot control state (caller/callee management, health) ─────────────
    let control_state = Arc::new(SipControlState {
        sip_port,
        callers: Default::default(),
        callees: Default::default(),
    });
    let control_router = sipbot_control_router(control_state);

    // Merge CC router with control router (CC routes take precedence)
    let merged_router = cc_router.merge(control_router);

    // ── Start TestPbx ──────────────────────────────────────────────────────
    let inject = TestPbxInject {
        proxy_config: Some(ProxyConfig {
            modules: Some(vec!["registrar".to_string(), "call".to_string()]),
            enable_latching: true,
            ..Default::default()
        }),
        routes: Some(vec![ivr_route]),
        queues: Some(queues),
        agent_registry: Some(registry),
        ws_port: Some(pick_unused_port().expect("no free WS port")),
        ws_handler: Some("/ws".to_string()),
        enable_cc_api: false,
        extra_router: Some(merged_router),
        ..Default::default()
    };

    let pbx = TestPbx::start_with_inject(sip_port, inject).await;

    let info = HarnessInfo {
        sip_port,
        ws_url: pbx.ws_url.clone().unwrap_or_default(),
        rwi_url: pbx.rwi_url.clone(),
        http_port: pbx.http_port,
        pid: std::process::id(),
        ivr_entry: format!("sip:ivrcc@127.0.0.1:{}", sip_port),
        db_type: "sqlite".to_string(),
    };
    println!("{}", serde_json::to_string(&info).unwrap());

    let ct = cancel_token.child_token();
    tokio::select! {
        _ = ct.cancelled() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    pbx.stop();
}
