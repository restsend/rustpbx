//! Real-SIP-call RWI agent attribution E2E (r14).
//!
//! Direct softphone outbound INVITEs must attribute the call to the
//! originating agent from the very first event — `call_created` carries
//! `agent_id`/`agent_name` (seeded into `CallMeta` before the event fans
//! out), and the session attribution is pinned so the CC hook's
//! ringing/connect attribution (priority 1) survives From overrides
//! (hotline) — repairing busy marking / CDR agent for those calls.
//!
//! Guards pinned here as regressions:
//! - internal (agent→agent) calls stay unattributed on `call_created`
//!   (direction guard; callee-side hook attribution owns those);
//! - callers the registry cannot confirm never get attributed.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rustpbx::call::app::agent_registry::{
    AgentRecord, AgentRegistry, PresenceState, RoutingStrategy,
};
use rustpbx::config::{LocatorWebhookConfig, MediaProxyMode, ProxyConfig};
use rustpbx::proxy::proxy_call::session_hooks::{CallSessionContext, CallSessionHook};
use rustpbx::proxy::routing::{
    DestConfig, MatchConditions, RouteAction, RouteRule, TrunkConfig,
};
use rustpbx::rwi::{RwiGateway, RwiGatewayRef, webhook::start_rwi_webhook_handler};
use tokio::sync::Mutex;
use tokio::time::sleep;

use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::webhook_capture::WebhookCapture;

/// Minimal static agent registry (the in-core `MemoryRegistry` is
/// `#[cfg(test)]`-only): just enough for attribution lookups — `register`
/// fills a map, `get_agent` reads it, everything queue/ACD-related is inert.
#[derive(Default)]
struct StaticRegistry {
    agents: std::sync::Mutex<std::collections::HashMap<String, AgentRecord>>,
}

impl StaticRegistry {
    fn with_agents(agents: &[(&str, &str)]) -> Arc<Self> {
        let reg = Self::default();
        for (id, name) in agents {
            reg.agents.lock().unwrap().insert(
                (*id).to_string(),
                AgentRecord {
                    agent_id: (*id).to_string(),
                    display_name: (*name).to_string(),
                    uri: format!("sip:{id}@127.0.0.1"),
                    skills: vec![],
                    max_concurrency: 1,
                    current_calls: 0,
                    presence: PresenceState::Idle,
                    last_state_change: std::time::Instant::now(),
                    total_calls_handled: 0,
                    total_talk_time_secs: 0,
                    last_call_end: None,
                },
            );
        }
        Arc::new(reg)
    }
}

#[async_trait]
impl AgentRegistry for StaticRegistry {
    async fn register(
        &self,
        agent_id: String,
        display_name: String,
        uri: String,
        skills: Vec<String>,
        max_concurrency: u32,
    ) -> anyhow::Result<()> {
        self.agents.lock().unwrap().insert(
            agent_id.clone(),
            AgentRecord {
                agent_id,
                display_name,
                uri,
                skills,
                max_concurrency,
                current_calls: 0,
                presence: PresenceState::Idle,
                last_state_change: std::time::Instant::now(),
                total_calls_handled: 0,
                total_talk_time_secs: 0,
                last_call_end: None,
            },
        );
        Ok(())
    }

    async fn unregister(&self, agent_id: &str) -> anyhow::Result<()> {
        self.agents
            .lock()
            .unwrap()
            .remove(agent_id)
            .map(|_| ())
            .ok_or_else(|| anyhow::anyhow!("agent {agent_id} not found"))
    }

    async fn get_agent(&self, agent_id: &str) -> Option<AgentRecord> {
        self.agents.lock().unwrap().get(agent_id).cloned()
    }

    async fn list_agents(&self) -> Vec<AgentRecord> {
        self.agents.lock().unwrap().values().cloned().collect()
    }

    async fn update_presence(&self, agent_id: &str, new_state: PresenceState) -> anyhow::Result<()> {
        let mut agents = self.agents.lock().unwrap();
        let agent = agents
            .get_mut(agent_id)
            .ok_or_else(|| anyhow::anyhow!("agent {agent_id} not found"))?;
        agent.presence = new_state;
        agent.last_state_change = std::time::Instant::now();
        Ok(())
    }

    async fn find_available_agents(&self, _required_skills: &[String]) -> Vec<AgentRecord> {
        Vec::new()
    }

    async fn select_agent(
        &self,
        _required_skills: &[String],
        _strategy: RoutingStrategy,
    ) -> Option<AgentRecord> {
        None
    }

    async fn resolve_target(&self, _target_uri: &str) -> Vec<String> {
        Vec::new()
    }
}

const ALICE_SDP: &str = "v=0\r\n\
    o=- 123456 123456 IN IP4 127.0.0.1\r\n\
    s=-\r\n\
    c=IN IP4 127.0.0.1\r\n\
    t=0 0\r\n\
    m=audio 12345 RTP/AVP 0 101\r\n\
    a=rtpmap:0 PCMU/8000\r\n\
    a=rtpmap:101 telephone-event/8000\r\n\
    a=sendrecv\r\n";

/// Poll the webhook capture until it has seen `event_type`.
async fn wait_webhook_envelope(
    capture: &WebhookCapture,
    event_type: &str,
) -> Option<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        {
            let received = capture.received.lock().unwrap();
            if let Some(ev) = received
                .iter()
                .find(|v| v["event_type"].as_str() == Some(event_type))
            {
                return Some(ev.clone());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

/// Records the agent attribution the session pins into the hook context —
/// what the CC hook reads at priority 1 to drive ringing/connect attribution
/// (busy marking, CDR agent). Captured at `on_call_ended` because it fires
/// for every session teardown, including failed outbound dials.
#[derive(Clone)]
struct AttributionHook {
    seen: Arc<Mutex<Vec<(String, Option<String>)>>>,
}

impl AttributionHook {
    fn new() -> Self {
        Self {
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    async fn contains_attribution(&self, agent_id: &str) -> bool {
        let seen = self.seen.lock().await;
        seen.iter().any(|(_, a)| a.as_deref() == Some(agent_id))
    }

    async fn wait_for_any_entry(&self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while self.seen.lock().await.is_empty() {
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            sleep(Duration::from_millis(50)).await;
        }
    }
}

#[async_trait]
impl CallSessionHook for AttributionHook {
    async fn on_call_ended(
        &self,
        ctx: &CallSessionContext,
        _reason: Option<&rustpbx::callrecord::CallRecordHangupReason>,
        _duration_secs: u64,
    ) {
        self.seen.lock().await.push((
            ctx.session_id.clone(),
            ctx.extensions.agent_attribution(),
        ));
    }
}

async fn start_server(
    agents: &[(&str, &str)],
    hook: AttributionHook,
) -> (WebhookCapture, Arc<E2eTestServer>) {
    let capture = WebhookCapture::start().await;
    let gateway: RwiGatewayRef = Arc::new(parking_lot::RwLock::new({
        let mut gw = RwiGateway::new();
        gw.set_webhook_tx(start_rwi_webhook_handler(
            LocatorWebhookConfig {
                url: capture.url.clone(),
                events: vec![],
                headers: None,
                timeout_ms: Some(5000),
                retries: None,
                track_queue_latency: None,
            },
            rustpbx::rwi::webhook::WEBHOOK_CHANNEL_SIZE,
        ));
        gw
    }));

    let registry = StaticRegistry::with_agents(agents);

    let mut config = ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(portpicker::pick_unused_port().unwrap_or(15060)),
        modules: Some(vec![
            "auth".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]),
        media_proxy: MediaProxyMode::All,
        ..Default::default()
    };
    config.route_originated_calls = false;
    // Production-shaped outbound path: `913*` external numbers route through
    // a carrier trunk. The trunk endpoint is intentionally dead (nothing
    // listens on port 1) — the leg dial fails fast AFTER the session (and
    // `call_created`) exist, which is exactly the window under test.
    config.trunks.insert(
        "testcarrier".to_string(),
        TrunkConfig {
            dest: "127.0.0.1:1".to_string(),
            backup_dest: None,
            username: None,
            password: None,
            codec: Vec::new(),
            disabled: None,
            max_calls: None,
            concurrent_call_limiter: None,
            max_cps: None,
            cps_limiter: None,
            weight: None,
            ..Default::default()
        },
    );
    config.routes = Some(vec![RouteRule {
        name: "test-external-outbound".to_string(),
        match_conditions: MatchConditions {
            to_user: Some("^913".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            action: Some("forward".to_string()),
            dest: Some(DestConfig::Single("testcarrier".to_string())),
            ..Default::default()
        },
        ..Default::default()
    }]);
    let mut users = crate::common::test_helpers::standard_test_users();
    for user in &mut users {
        user.is_support_webrtc = false;
    }
    let server = Arc::new(
        E2eTestServer::start_with_inject(
            config,
            E2eTestServerInject {
            bans: None,
                agent_registry: Some(registry.clone()),
                session_hook: Some(Arc::new(hook.clone())),
                users,
                rwi_gateway: Some(gateway),
                ..Default::default()
            },
        )
        .await
        .expect("E2E server start failed"),
    );
    (capture, server)
}

/// Agent dials an external (non-local) number: `call_created` must carry the
/// originating agent immediately (auth-user → registry resolution), and the
/// session attribution must be pinned into the hook context for the CC
/// hook's priority-1 read.
#[tokio::test]
async fn outbound_agent_call_created_carries_originating_agent() {
    let _ = tracing_subscriber::fmt::try_init();

    let hook = AttributionHook::new();
    let (capture, server) = start_server(&[("alice", "Alice")], hook.clone()).await;

    let alice = server
        .create_ua("alice")
        .await
        .expect("create alice failed");

    // External (non-local) destination → direction=Outbound. The dial itself
    // fails fast (no trunk / locator entry) — `call_created` is already out.
    let _ = alice.make_call("91300000000", Some(ALICE_SDP.to_string())).await;

    let created = wait_webhook_envelope(&capture, "call_created")
        .await
        .expect("webhook must receive call_created for the agent outbound INVITE");
    let event = &created["event"];
    assert_eq!(
        event["agent_id"].as_str(),
        Some("alice"),
        "call_created must carry the originating agent: {created}"
    );
    assert_eq!(
        event["agent_name"].as_str(),
        Some("Alice"),
        "call_created must carry the agent display name: {created}"
    );

    // Session attribution pinned → CC hook priority 1 (busy marking / CDR).
    hook.wait_for_any_entry().await;
    assert!(
        hook.contains_attribution("alice").await,
        "session hook must observe the pinned agent attribution: {:?}",
        hook.seen.lock().await
    );
}

/// Agent→agent (both registered, internal direction): `call_created` stays
/// unattributed — the direction guard leaves A2A attribution to the CC
/// hook's callee-side logic at ringing/connect.
#[tokio::test]
async fn internal_agent_to_agent_call_created_stays_unattributed() {
    let _ = tracing_subscriber::fmt::try_init();

    let hook = AttributionHook::new();
    let (capture, server) =
        start_server(&[("alice", "Alice"), ("bob", "Bob")], hook.clone()).await;

    let alice = server
        .create_ua("alice")
        .await
        .expect("create alice failed");
    let _bob = server.create_ua("bob").await.expect("create bob failed");

    // bob never answers — spawn the caller so the test only waits for
    // `call_created` (the runtime tears the pending dialog down at the end).
    let caller = alice.clone();
    let _call = rustpbx::utils::spawn(async move {
        let _ = caller.make_call("bob", Some(ALICE_SDP.to_string())).await;
    });

    let created = wait_webhook_envelope(&capture, "call_created")
        .await
        .expect("webhook must receive call_created for the internal call");
    let event = &created["event"];
    assert_ne!(
        event["agent_id"].as_str(),
        Some("alice"),
        "internal call must not attribute the caller agent on call_created: {created}"
    );
    assert_ne!(
        event["agent_id"].as_str(),
        Some("bob"),
        "internal call_created must not pre-attribute the callee either: {created}"
    );
}

/// A caller the registry cannot confirm (non-agent) dialing out: no
/// attribution anywhere — registry confirmation is mandatory.
#[tokio::test]
async fn non_agent_outbound_call_created_stays_unattributed() {
    let _ = tracing_subscriber::fmt::try_init();

    let hook = AttributionHook::new();
    // Only bob is a registered agent; alice is a plain user.
    let (capture, server) = start_server(&[("bob", "Bob")], hook.clone()).await;

    let alice = server
        .create_ua("alice")
        .await
        .expect("create alice failed");

    let _ = alice.make_call("91300000000", Some(ALICE_SDP.to_string())).await;

    let created = wait_webhook_envelope(&capture, "call_created")
        .await
        .expect("webhook must receive call_created");
    assert_ne!(
        created["event"]["agent_id"].as_str(),
        Some("alice"),
        "non-agent caller must not be attributed: {created}"
    );

    hook.wait_for_any_entry().await;
    assert!(
        !hook.contains_attribution("alice").await,
        "session attribution must stay empty for non-agent callers: {:?}",
        hook.seen.lock().await
    );
}
