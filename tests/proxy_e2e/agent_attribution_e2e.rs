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
use rustpbx::config::{LocatorWebhookConfig, MediaProxyMode, ProxyConfig};
use rustpbx::proxy::proxy_call::session_hooks::{CallSessionContext, CallSessionHook};
use rustpbx::proxy::routing::{
    DestConfig, MatchConditions, RouteAction, RouteRule, TrunkConfig,
};
use rustpbx::rwi::{RwiGateway, RwiGatewayRef, webhook::start_rwi_webhook_handler};
use tokio::sync::Mutex;
use tokio::time::sleep;

use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent};
use crate::common::webhook_capture::WebhookCapture;

use crate::common::agent_registry::StaticRegistry;

const ALICE_SDP: &str = "v=0\r\n\
    o=- 123456 123456 IN IP4 127.0.0.1\r\n\
    s=-\r\n\
    c=IN IP4 127.0.0.1\r\n\
    t=0 0\r\n\
    m=audio 12345 RTP/AVP 0 101\r\n\
    a=rtpmap:0 PCMU/8000\r\n\
    a=rtpmap:101 telephone-event/8000\r\n\
    a=sendrecv\r\n";

/// Poll the webhook capture until it has seen the SESSION-scoped
/// `call_hangup` (`scope == "call"`). A CC-role leg may emit its leg-scoped
/// teardown first, so filtering on event type alone would race.
async fn wait_session_hangup(capture: &WebhookCapture) -> Option<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        {
            let received = capture.received.lock().unwrap();
            if let Some(ev) = received.iter().find(|v| {
                v["event_type"].as_str() == Some("call_hangup")
                    && v["event"]["scope"].as_str() == Some("call")
            }) {
                return Some(ev.clone());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

/// Poll the unregistered customer UA until the outbound INVITE arrives.
async fn wait_incoming_call(ua: &TestUa) -> Option<rsipstack::dialog::DialogId> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(events) = ua.process_dialog_events().await {
            for event in events {
                if let TestUaEvent::IncomingCall(id, _) = event {
                    return Some(id);
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

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
    start_server_with_carrier(agents, hook, "127.0.0.1:1").await
}

/// Same server, but the `913*` carrier trunk points at `carrier_dest` so a
/// test can put a real (unregistered) customer UA on the other end and drive
/// the first terminal BYE from either side.
async fn start_server_with_carrier(
    agents: &[(&str, &str)],
    hook: AttributionHook,
    carrier_dest: &str,
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
                require_event_ack: None,
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
            dest: carrier_dest.to_string(),
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

/// Direct softphone outbound to an EXTERNAL customer that ANSWERS, then the
/// CUSTOMER hangs up first. The seat is the A/caller leg (agent-attributed),
/// the customer is the B/callee leg — the session-scoped `call_hangup` must
/// say `hangup_by=callee` (not the CC-centric `agent`) and the CDR metadata
/// must carry the same value.
#[tokio::test]
async fn outbound_customer_hangup_reports_callee_in_webhook_and_cdr() {
    let _ = tracing_subscriber::fmt::try_init();

    let customer_port = portpicker::pick_unused_port().expect("free customer port");
    let hook = AttributionHook::new();
    let (capture, server) = start_server_with_carrier(
        &[("alice", "Alice")],
        hook,
        &format!("127.0.0.1:{customer_port}"),
    )
    .await;

    let alice = server.create_ua("alice").await.expect("create alice failed");
    let mut customer = TestUa::new(TestUaConfig {
        webrtc: false,
        username: "customer".to_string(),
        password: String::new(),
        realm: "127.0.0.1".to_string(),
        local_port: customer_port,
        proxy_addr: server.proxy_addr,
    });
    customer.start().await.expect("customer UA start");
    sleep(Duration::from_millis(200)).await;

    let caller = {
        let a = alice.clone();
        rustpbx::utils::spawn(async move { a.make_call("91300000000", Some(ALICE_SDP.to_string())).await })
    };
    let dialog = wait_incoming_call(&customer)
        .await
        .expect("customer must receive the outbound INVITE");
    customer
        .answer_call(&dialog, Some(ALICE_SDP.to_string()))
        .await
        .expect("customer answer");
    wait_webhook_envelope(&capture, "call_answered")
        .await
        .expect("call must connect before the hangup assertion");

    // Customer (B/callee dialog remote) hangs up first.
    customer.hangup(&dialog).await.expect("customer BYE");
    let hangup = wait_session_hangup(&capture)
        .await
        .expect("session-scoped call_hangup");
    assert_eq!(
        hangup["event"]["hangup_by"].as_str(),
        Some("callee"),
        "external customer hangup must not be labeled agent: {hangup}"
    );
    assert_eq!(hangup["event"]["reason"].as_str(), Some("callee"));

    // Same value on the CDR surface (metadata.hangup_by).
    let call_id = hangup["event"]["call_id"]
        .as_str()
        .expect("call_hangup call_id")
        .to_string();
    let records = server
        .cdr_capture
        .wait_for_records(1, Duration::from_secs(8))
        .await
        .expect("CDR for the answered outbound call");
    let record = records
        .iter()
        .find(|r| r.call_id == call_id)
        .expect("CDR must match the call_hangup call_id");
    assert_eq!(
        record
            .details
            .metadata
            .as_ref()
            .and_then(|m| m.get("hangup_by"))
            .and_then(|v| v.as_str()),
        Some("callee"),
        "CDR metadata.hangup_by must agree with the webhook"
    );

    // The customer callee leg must never be emitted as an agent leg (the
    // former `accept_call` served-agent stamp mislabeled it with the
    // ORIGINATING agent on outbound calls). The caller slot is the seat — a
    // legitimate agent leg there is not this bug.
    let customer_agent_leg_hangups: Vec<_> = capture
        .received
        .lock()
        .unwrap()
        .iter()
        .filter(|v| {
            v["event_type"].as_str() == Some("call_hangup")
                && v["event"]["leg_role"].as_str() == Some("agent")
                && v["event"]["leg_id"].as_str() != Some("caller")
        })
        .cloned()
        .collect();
    assert!(
        customer_agent_leg_hangups.is_empty(),
        "the dialed customer leg must not carry leg_role=agent: {customer_agent_leg_hangups:?}"
    );

    let _ = tokio::time::timeout(Duration::from_secs(2), caller).await;
    server.stop();
}

/// Symmetric shape: the SEAT (A/caller leg) hangs up first. The SIP reason is
/// `caller` (UAS caller dialog remote BYE) but the business initiator must be
/// `agent` — webhook and CDR agree.
#[tokio::test]
async fn outbound_seat_hangup_reports_agent_in_webhook_and_cdr() {
    let _ = tracing_subscriber::fmt::try_init();

    let customer_port = portpicker::pick_unused_port().expect("free customer port");
    let hook = AttributionHook::new();
    let (capture, server) = start_server_with_carrier(
        &[("alice", "Alice")],
        hook,
        &format!("127.0.0.1:{customer_port}"),
    )
    .await;

    let alice = server.create_ua("alice").await.expect("create alice failed");
    let mut customer = TestUa::new(TestUaConfig {
        webrtc: false,
        username: "customer".to_string(),
        password: String::new(),
        realm: "127.0.0.1".to_string(),
        local_port: customer_port,
        proxy_addr: server.proxy_addr,
    });
    customer.start().await.expect("customer UA start");
    sleep(Duration::from_millis(200)).await;

    let caller = {
        let a = alice.clone();
        rustpbx::utils::spawn(async move { a.make_call("91300000000", Some(ALICE_SDP.to_string())).await })
    };
    let dialog = wait_incoming_call(&customer)
        .await
        .expect("customer must receive the outbound INVITE");
    customer
        .answer_call(&dialog, Some(ALICE_SDP.to_string()))
        .await
        .expect("customer answer");
    wait_webhook_envelope(&capture, "call_answered")
        .await
        .expect("call must connect before the hangup assertion");

    let alice_dialog = tokio::time::timeout(Duration::from_secs(10), caller)
        .await
        .expect("make_call must return after the answer")
        .expect("caller task")
        .expect("make_call");
    alice.hangup(&alice_dialog).await.expect("seat BYE");

    let hangup = wait_session_hangup(&capture)
        .await
        .expect("session-scoped call_hangup");
    assert_eq!(
        hangup["event"]["hangup_by"].as_str(),
        Some("agent"),
        "seat (agent) hangup must be agent, not the SIP positional caller: {hangup}"
    );
    assert_eq!(hangup["event"]["reason"].as_str(), Some("caller"));

    let call_id = hangup["event"]["call_id"]
        .as_str()
        .expect("call_hangup call_id")
        .to_string();
    let records = server
        .cdr_capture
        .wait_for_records(1, Duration::from_secs(8))
        .await
        .expect("CDR for the answered outbound call");
    let record = records
        .iter()
        .find(|r| r.call_id == call_id)
        .expect("CDR must match the call_hangup call_id");
    assert_eq!(
        record
            .details
            .metadata
            .as_ref()
            .and_then(|m| m.get("hangup_by"))
            .and_then(|v| v.as_str()),
        Some("agent"),
        "CDR metadata.hangup_by must agree with the webhook"
    );

    server.stop();
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
