//! e2e: `caller → IVR (press 1) → queue → agent answers → agent REFERs the
//! customer BACK to the IVR → caller finishes in the IVR`, under the
//! "agent-only" recording configuration:
//!
//! - `[recording] auto_start = false` — the IVR stage records NOTHING
//! - cc.toml `[recording] record_on_agent_connect = true`
//!   (`CcConfig::default()`) — a `segment_type = "agent"` segment starts when
//!   the agent connects and stops when the agent's leg leaves
//!
//! Pins the contract that the agent's REFER-exit is a first-class segment
//! boundary:
//!
//! 1. **Flow correctness** — the REFER hands the call back to the IVR in the
//!    SAME session: the agent leg departs after its BYE, the IVR successor
//!    serves the caller (root node entered again), DTMF still works, and the
//!    call ends cleanly only when the caller finishes.
//! 2. **Exactly ONE recording artifact** — the agent segment. No full-call
//!    recording, no IVR-stage recording: a single `record_stopped` whose
//!    filename ends `_<agent_id>.wav`, and exactly one `.wav` on disk.
//! 3. **Segment boundaries** — the recording covers the agent talk window and
//!    stops at the agent BYE; it must NOT keep rolling through the post-REFER
//!    IVR stage until call end.
//! 4. **Content-level proof** — the WAV is stereo and BOTH channels carry
//!    the conversation tones (caller tone on the left/leg-A ingress channel,
//!    agent tone on the right/egress channel), so a silent or empty file can
//!    never fake a pass.
//! 5. **CDR contract** — `recorder[]` + `metadata.recording_segments` carry
//!    the single agent segment (`segment_type = "agent"`, `label = agent`).

use anyhow::{Result, anyhow};
use rustpbx::addons::cc::acd::{AcdConfig, AcdEngine};
use rustpbx::addons::cc::agent::AgentRegistry as CcAgentRegistry;
use rustpbx::addons::cc::agent_registry_adapter::CcAgentRegistryAdapter;
use rustpbx::addons::cc::cc_call_session_hook::CcCallSessionHook;
use rustpbx::addons::cc::config::CcConfig;
use rustpbx::addons::cc::metrics::MetricsCollector;
use rustpbx::addons::cc::skill_group::CreateSkillGroupRequest;
use rustpbx::call::user::SipUser;
use rustpbx::config::{LocatorWebhookConfig, ProxyConfig, RecordingPolicy};
use rustpbx::proxy::active_call_registry::ActiveProxyCallRegistry;
use rustpbx::proxy::routing::{
    MatchConditions, QueueDialMode, RouteAction, RouteQueueConfig, RouteQueueStrategyConfig,
    RouteQueueTargetConfig, RouteRule,
};
use rustpbx::rwi::{RwiGateway, RwiGatewayRef, webhook::start_rwi_webhook_handler};
use sea_orm::Database;
use sea_orm_migration::MigratorTrait;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::time::sleep;

use crate::common::audio_analysis::rms;
use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::rtp_utils::{RtpPacket, extract_media_endpoint};
use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent, create_test_sdp_answer};
use crate::common::webhook_capture::WebhookCapture;

const IVR_NUMBER: &str = "9200";
const QUEUE_NAME: &str = "refer_back_q";
const SKILL_GROUP: &str = "sg_refer_back";
const AGENT: &str = "bob";
const CALLER: &str = "caller";

/// How long customer + agent talk (and record) before the agent REFERs back.
const TALK_SECS: u64 = 2;
/// Post-BYE settle window: the agent segment must already be closed while the
/// call keeps running in the IVR — a segment that rolls until call end grows
/// past the duration bound below.
const POST_BYE_SETTLE: Duration = Duration::from_millis(2000);

/// Distinct μ-law tones per party — both decode to loud samples (|amp| > 3000)
/// so the per-channel RMS assertions have real signal, not quantization noise.
const CALLER_TONE_BYTE: u8 = 0x33;
const AGENT_TONE_BYTE: u8 = 0x55;

const WEBHOOK_EVENTS: &[&str] = &[
    "call_created",
    "call_ringing",
    "call_answered",
    "call_hangup",
    "queue_joined",
    "queue_agent_offered",
    "queue_agent_connected",
    "queue_left",
    "ivr_node_entered",
    "ivr_node_exited",
    "call_transferred",
    "record_stopped",
];

fn ivr_toml() -> String {
    // The greeting file does not exist → the tree IVR skips playback and
    // waits for DTMF immediately (no TTS / audio dependency in CI).
    // key 1 → queue (agent stage); key 9 → hangup (the caller's exit after
    // the agent REFERs them back into this same IVR).
    format!(
        r#"
[ivr]
name = "refer-back-ivr"

[ivr.root]
greeting = "sounds/definitely-missing-refer-back.wav"
timeout_ms = 15000
max_retries = 3

[[ivr.root.entries]]
key = "1"
action = {{ type = "transfer", target = "queue:{QUEUE_NAME}" }}

[[ivr.root.entries]]
key = "9"
action = {{ type = "hangup" }}
"#
    )
}

fn proxy_config(ivr_file: &std::path::Path, record_path: &str) -> ProxyConfig {
    let mut config = ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(0),
        modules: Some(vec![
            "auth".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]),
        ..Default::default()
    };

    // "Agent-only" recording policy: enabled for on-demand segments, but NO
    // auto start — the IVR stage must not record, only the cc agent segment.
    config.recording = Some(RecordingPolicy {
        enabled: Some(true),
        auto_start: Some(false),
        path: Some(record_path.to_string()),
        ..Default::default()
    });

    config.queues.insert(
        QUEUE_NAME.to_string(),
        RouteQueueConfig {
            name: Some(QUEUE_NAME.to_string()),
            strategy: RouteQueueStrategyConfig {
                mode: QueueDialMode::Sequential,
                wait_timeout_secs: Some(30),
                targets: vec![RouteQueueTargetConfig {
                    uri: format!("skill-group:{SKILL_GROUP}"),
                    label: None,
                }],
            },
            accept_immediately: false,
            ..Default::default()
        },
    );

    config.routes = Some(vec![RouteRule {
        name: "route_to_refer_back_ivr".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some(IVR_NUMBER.to_string()),
            ..Default::default()
        },
        action: RouteAction {
            app: Some("ivr".to_string()),
            app_params: Some(serde_json::json!({
                "file": ivr_file.to_string_lossy(),
            })),
            ..Default::default()
        },
        ..Default::default()
    }]);

    config
}

struct ReferBackHarness {
    server: E2eTestServer,
    cc_registry: Arc<CcAgentRegistry>,
    /// Keeps the record dir (and the IVR temp file) alive for the whole test.
    _record_dir: tempfile::TempDir,
    _ivr_file: tempfile::NamedTempFile,
}

async fn start_harness(capture: &WebhookCapture) -> Result<ReferBackHarness> {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    rustpbx::addons::cc::migration::Migrator::up(&db, None).await.unwrap();

    rustpbx::addons::cc::skill_group::create_skill_group(
        &db,
        CreateSkillGroupRequest {
            skill_group_id: SKILL_GROUP.to_string(),
            display_name: Some("Refer Back Q".to_string()),
            skills_required: vec!["support".to_string()],
            overflow_groups: vec![],
            sla_target_secs: 30,
            max_wait_secs: 120,
            metadata: None,
        },
    )
    .await
    .unwrap();

    let cc_registry = Arc::new(CcAgentRegistry::with_db(db.clone()));
    cc_registry
        .register(AGENT.to_string(), vec!["support".to_string()], 1)
        .await
        .unwrap();
    cc_registry
        .update_status(AGENT, rustpbx::addons::cc::agent::AgentStatus::Idle)
        .await
        .unwrap();

    let adapter = Arc::new(CcAgentRegistryAdapter::new(
        cc_registry.clone(),
        Arc::new(AcdEngine::new(AcdConfig {
            enabled: false,
            ..AcdConfig::default()
        })),
        "localhost",
    ));

    let gateway: RwiGatewayRef = Arc::new(parking_lot::RwLock::new({
        let mut gw = RwiGateway::new();
        gw.set_webhook_tx(start_rwi_webhook_handler(
            LocatorWebhookConfig {
                url: capture.url.clone(),
                events: WEBHOOK_EVENTS.iter().map(|s| s.to_string()).collect(),
                headers: None,
                timeout_ms: Some(5000),
                retries: None,
                track_queue_latency: None,
            },
            rustpbx::rwi::webhook::WEBHOOK_CHANNEL_SIZE,
        ));
        gw
    }));

    let record_dir = tempfile::tempdir()?;
    let ivr_file = tempfile::NamedTempFile::with_suffix(".toml")?;
    std::fs::write(ivr_file.path(), ivr_toml())?;

    let mut proxy_config = proxy_config(ivr_file.path(), &record_dir.path().to_string_lossy());
    proxy_config.ensure_user = Some(false);
    proxy_config.enable_latching = false;

    let users = vec![
        SipUser {
            id: 1,
            username: AGENT.to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        },
        SipUser {
            id: 2,
            username: CALLER.to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        },
    ];

    let call_registry_container: Arc<tokio::sync::RwLock<Option<Arc<ActiveProxyCallRegistry>>>> =
        Arc::new(tokio::sync::RwLock::new(None));
    let session_hook = CcCallSessionHook::new(cc_registry.clone(), Arc::new(MetricsCollector::new()))
        // Production default: `[recording] record_on_agent_connect = true`.
        .with_cc_config(Arc::new(CcConfig::default()))
        .with_call_registry_shared(call_registry_container.clone());

    let server = E2eTestServer::start_with_inject(
        proxy_config,
        E2eTestServerInject {
            queue_enricher: None,
            users,
            session_hook: Some(Arc::new(session_hook)),
            agent_registry: Some(adapter),
            rwi_gateway: Some(gateway),
            #[cfg(feature = "addon-cc")]
            cc_policy_db: None,
        },
    )
    .await?;
    *call_registry_container.write().await = Some(server.registry.clone());

    Ok(ReferBackHarness {
        server,
        cc_registry,
        _record_dir: record_dir,
        _ivr_file: ivr_file,
    })
}

fn make_ua(proxy_addr: std::net::SocketAddr, username: &str) -> TestUa {
    TestUa::new(TestUaConfig {
        webrtc: false,
        username: username.to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(29500),
        proxy_addr,
    })
}

/// Continuous caller RTP: PCMU packets filled with [`CALLER_TONE_BYTE`] —
/// this is the customer-side audio the recording must capture.
async fn spawn_caller_rtp(socket: Arc<UdpSocket>, target: std::net::SocketAddr) {
    let mut seq: u16 = 100;
    let mut ts: u32 = 1000;
    loop {
        let packet = RtpPacket::new(0, seq, ts, 0x1111, vec![CALLER_TONE_BYTE; 160]);
        if socket.send_to(&packet.encode(), target).await.is_err() {
            break;
        }
        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(160);
        sleep(Duration::from_millis(20)).await;
    }
}

fn count_events(capture: &WebhookCapture, event_type: &str) -> usize {
    capture
        .received
        .lock()
        .unwrap()
        .iter()
        .filter(|v| v["event_type"].as_str() == Some(event_type))
        .count()
}

async fn wait_webhook_event(
    capture: &WebhookCapture,
    event_type: &str,
    timeout: Duration,
) -> Option<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(ev) = capture
            .received
            .lock()
            .unwrap()
            .iter()
            .find(|v| v["event_type"].as_str() == Some(event_type))
            .cloned()
        {
            return Some(ev);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn ms_since(epoch: &Instant) -> u64 {
    epoch.elapsed().as_millis() as u64
}

/// All tests in this file run the full SIP session + queue + IVR future
/// chain, which in debug builds needs more than the default 2 MiB tokio
/// thread stack (production uses 8 MiB, see src/bin/rustpbx.rs). Drive each
/// scenario on a big-stack worker thread so no RUST_MIN_STACK env var is
/// needed.
fn run_with_big_stack<F>(fut: F)
where
    F: std::future::Future<Output = Result<()>> + Send + 'static,
{
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_stack_size(32 * 1024 * 1024)
        .build()
        .expect("failed to build test runtime");
    rt.block_on(async move {
        tokio::spawn(fut)
            .await
            .expect("test task panicked")
            .expect("e2e refer-back recording test failed");
    });
}

/// IVR → queue → agent → agent REFERs the caller back to the IVR. With the
/// agent-only recording configuration exactly one agent segment must exist,
/// bounded by [agent connect, agent BYE].
#[test]
fn test_ivr_agent_refer_back_to_ivr_records_only_agent_segment() {
    run_with_big_stack(test_ivr_agent_refer_back_to_ivr_records_only_agent_segment_impl());
}

async fn test_ivr_agent_refer_back_to_ivr_records_only_agent_segment_impl() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    let capture = WebhookCapture::start().await;
    let harness = start_harness(&capture).await?;
    let proxy_addr = harness.server.proxy_addr;
    let epoch = Instant::now();

    // ── Caller UA + media ─────────────────────────────────────────────────
    let caller_rtp = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
    let mut caller = make_ua(proxy_addr, CALLER);
    caller.start().await?;
    caller.register().await?;
    sleep(Duration::from_millis(200)).await;

    let caller_port = caller_rtp.local_addr()?.port();
    let caller_sdp = format!(
        "v=0\r\n\
         o=- 123456 123456 IN IP4 127.0.0.1\r\n\
         s=-\r\n\
         c=IN IP4 127.0.0.1\r\n\
         t=0 0\r\n\
         m=audio {caller_port} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=sendrecv\r\n"
    );

    // ── Agent pump: answers, then on request REFERs the caller back to the
    //    IVR number and hangs its own leg after the final NOTIFY ───────────
    let agent_rtp = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
    let agent_media_endpoint: Arc<std::sync::Mutex<Option<std::net::SocketAddr>>> =
        Arc::new(std::sync::Mutex::new(None));
    let established = Arc::new(AtomicUsize::new(0));
    let refer_requested = Arc::new(AtomicUsize::new(0));
    let bye_at_ms = Arc::new(AtomicU64::new(0));

    let mut pump_ua = make_ua(proxy_addr, AGENT);
    pump_ua.start().await?;
    pump_ua.register().await?;
    let pump = {
        let ua = pump_ua.clone();
        let agent_rtp_port = agent_rtp.local_addr()?.port();
        let endpoint_slot = agent_media_endpoint.clone();
        let established = established.clone();
        let refer_requested = refer_requested.clone();
        let bye_at_ms = bye_at_ms.clone();
        let proxy = proxy_addr;
        tokio::spawn(async move {
            let mut active_dialog: Option<rsipstack::dialog::DialogId> = None;
            loop {
                match ua.process_dialog_events().await {
                    Ok(events) => {
                        for ev in events {
                            match ev {
                                TestUaEvent::IncomingCall(dialog_id, offer) => {
                                    let _ = ua.ring_call(&dialog_id).await;
                                    sleep(Duration::from_millis(300)).await;
                                    if let Some(offer) = offer.as_deref() {
                                        if let Some(endpoint) = extract_media_endpoint(offer) {
                                            *endpoint_slot.lock().unwrap() = Some(endpoint);
                                        }
                                    }
                                    let sdp = offer
                                        .as_deref()
                                        .map(|o| create_test_sdp_answer(o, "127.0.0.1", agent_rtp_port))
                                        .unwrap_or_else(|| {
                                            crate::common::test_ua::create_test_sdp("127.0.0.1", agent_rtp_port, false)
                                        });
                                    let _ = ua.answer_call(&dialog_id, Some(sdp)).await;
                                    active_dialog = Some(dialog_id);
                                }
                                TestUaEvent::CallEstablished(_) => {
                                    established.fetch_add(1, Ordering::Relaxed);
                                }
                                _ => {}
                            }
                        }
                        if refer_requested.load(Ordering::Relaxed) == 1
                            && let Some(dialog_id) = active_dialog.clone()
                        {
                            // Agent transfers the customer BACK to the IVR.
                            let target = format!("sip:{IVR_NUMBER}@{proxy}");
                            match ua.send_refer(&dialog_id, &target).await {
                                Ok(202) => {}
                                other => {
                                    panic!("REFER to IVR must be accepted with 202, got {other:?}");
                                }
                            }
                            // Wait for the final NOTIFY; the IVR handoff must
                            // only complete when the agent's own BYE arrives.
                            let deadline = Instant::now() + Duration::from_secs(8);
                            loop {
                                let mut notified = false;
                                for ev in ua.process_dialog_events().await.unwrap_or_default() {
                                    if let TestUaEvent::ReferNotify(_, body, state) = ev {
                                        if state.starts_with("terminated") {
                                            assert!(
                                                body.starts_with("SIP/2.0 200"),
                                                "final NOTIFY must report success: {body}"
                                            );
                                            notified = true;
                                        }
                                    }
                                }
                                refer_requested.store(2, Ordering::Relaxed);
                                if notified || Instant::now() > deadline {
                                    break;
                                }
                                sleep(Duration::from_millis(20)).await;
                            }
                            bye_at_ms.store(ms_since(&epoch), Ordering::Relaxed);
                            let _ = ua.hangup(&dialog_id).await;
                        }
                    }
                    Err(_) => break,
                }
                sleep(Duration::from_millis(30)).await;
            }
        })
    };

    // ── Caller → IVR ──────────────────────────────────────────────────────
    let dialog = caller.make_call(IVR_NUMBER, Some(caller_sdp.clone())).await?;
    wait_webhook_event(&capture, "ivr_node_entered", Duration::from_secs(8))
        .await
        .expect("IVR root node must be entered");

    // Caller media endpoint (the PBX leg) — start the caller tone loop.
    let answer_sdp = caller
        .get_negotiated_answer_sdp(&dialog)
        .await
        .ok_or_else(|| anyhow!("caller answer SDP must be negotiated"))?;
    let pbx_caller_endpoint = extract_media_endpoint(&answer_sdp)
        .ok_or_else(|| anyhow!("caller answer SDP has no media endpoint"))?;
    let caller_rtp_task = tokio::spawn(spawn_caller_rtp(caller_rtp.clone(), pbx_caller_endpoint));

    let original_session_id = harness
        .server
        .get_active_calls()
        .first()
        .map(|c| c.session_id.clone())
        .ok_or_else(|| anyhow!("active call must exist while the IVR runs"))?;

    // ── IVR key 1 → queue → agent connects ───────────────────────────────
    caller.send_dtmf_info(&dialog, "1").await?;
    wait_webhook_event(&capture, "queue_joined", Duration::from_secs(8))
        .await
        .expect("pressing 1 must join the queue");
    let connected = wait_webhook_event(&capture, "queue_agent_connected", Duration::from_secs(15))
        .await
        .expect("agent must be dialed and answer (queue_agent_connected)");
    assert_eq!(
        connected["event"]["agent_id"].as_str(),
        Some(AGENT),
        "queue_agent_connected must attribute the agent: {connected}"
    );

    let established_deadline = Instant::now() + Duration::from_secs(5);
    while established.load(Ordering::Relaxed) == 0 && Instant::now() < established_deadline {
        sleep(Duration::from_millis(30)).await;
    }
    assert_eq!(
        established.load(Ordering::Relaxed),
        1,
        "agent leg must be established with the caller"
    );
    let connect_at = Instant::now();
    let connect_ms = (connect_at - epoch).as_millis() as u64;

    // ── Agent talk window: agent tone toward the PBX (recorded on the
    //    egress channel). Caller tone keeps flowing from its own task. ─────
    let agent_endpoint = agent_media_endpoint
        .lock()
        .unwrap()
        .ok_or_else(|| anyhow!("agent leg media endpoint unknown"))?;
    {
        let mut seq: u16 = 500;
        let mut ts: u32 = 5000;
        for _ in 0..(TALK_SECS * 50) {
            let packet = RtpPacket::new(0, seq, ts, 0x2222, vec![AGENT_TONE_BYTE; 160]);
            agent_rtp.send_to(&packet.encode(), agent_endpoint).await?;
            seq = seq.wrapping_add(1);
            ts = ts.wrapping_add(160);
            sleep(Duration::from_millis(20)).await;
        }
    }

    // ── Agent REFERs the customer back to the IVR ─────────────────────────
    refer_requested.store(1, Ordering::Relaxed);
    let bye_deadline = Instant::now() + Duration::from_secs(10);
    while bye_at_ms.load(Ordering::Relaxed) == 0 && Instant::now() < bye_deadline {
        sleep(Duration::from_millis(30)).await;
    }
    let bye_at = bye_at_ms.load(Ordering::Relaxed);
    assert!(bye_at > 0, "agent pump must complete the REFER and hang up");

    // ── Flow: the call continues in the SAME session, back in the IVR ─────
    // The successor IVR re-enters the root node.
    let ivr_reenter_deadline = Instant::now() + Duration::from_secs(6);
    while count_events(&capture, "ivr_node_entered") < 2
        && Instant::now() < ivr_reenter_deadline
    {
        sleep(Duration::from_millis(50)).await;
    }
    assert!(
        count_events(&capture, "ivr_node_entered") >= 2,
        "the REFER must hand the call back to the IVR (second ivr_node_entered)"
    );
    let transferred = capture
        .received
        .lock()
        .unwrap()
        .iter()
        .find(|v| {
            v["event_type"].as_str() == Some("call_transferred")
                && v["event"]["transfer_target"]
                    .as_str()
                    .is_some_and(|t| t.contains(IVR_NUMBER))
        })
        .cloned()
        .ok_or_else(|| anyhow!("call_transferred must report the REFER handoff"))?;
    assert!(
        transferred["event"]["transfer_target"]
            .as_str()
            .is_some_and(|t| t.contains(IVR_NUMBER)),
        "transfer_target must carry the REFER target: {transferred}"
    );

    // The session survives the agent's departure.
    let calls = harness.server.get_active_calls();
    assert_eq!(calls.len(), 1, "only the original session must remain");
    assert_eq!(
        calls[0].session_id, original_session_id,
        "the REFER-to-IVR handoff must stay in the original session"
    );

    // ── Recording boundary: the agent segment must ALREADY be closed while
    //    the call keeps running in the IVR ─────────────────────────────────
    let stopped = wait_webhook_event(&capture, "record_stopped", Duration::from_secs(6))
        .await
        .ok_or_else(|| anyhow!("record_stopped for the agent segment is missing"))?;
    assert_eq!(
        count_events(&capture, "record_stopped"),
        1,
        "exactly ONE recording segment (the agent's) must exist"
    );
    let filename = stopped["event"]["filename"]
        .as_str()
        .ok_or_else(|| anyhow!("record_stopped missing filename: {stopped}"))?
        .to_string();
    assert!(
        filename.ends_with(&format!("_{AGENT}.wav")),
        "record_stopped must be the agent segment owned by {AGENT}: {stopped}"
    );
    let recorded_secs = stopped["event"]["duration_secs"]
        .as_i64()
        .ok_or_else(|| anyhow!("record_stopped missing duration_secs: {stopped}"))?;

    // Lower bound: the segment covers the whole agent talk window.
    assert!(
        recorded_secs >= TALK_SECS as i64 - 1,
        "agent recording ran only {recorded_secs}s — shorter than the {TALK_SECS}s talk window"
    );
    // Upper bound: the segment ends at the agent BYE; it must NOT keep
    // rolling through the post-REFER IVR stage until call end.
    let agent_stage_secs = bye_at.saturating_sub(connect_ms) as f64 / 1000.0;
    assert!(
        recorded_secs as f64 <= agent_stage_secs + 3.0,
        "agent recording ran {recorded_secs}s but the agent stage was {agent_stage_secs:.1}s — \
         the segment kept rolling after the agent REFER/hangup"
    );

    // Give the post-BYE settle window a chance to expose a still-running
    // recorder: the WAV must not grow after the agent left.
    sleep(POST_BYE_SETTLE).await;

    // ── Caller finishes the call in the IVR (key 9 → hangup) ──────────────
    caller.send_dtmf_info(&dialog, "9").await?;
    // The agent-leg departure already emitted its leg-scoped call_hangup;
    // select the session-level event (no leg_id).
    let hangup_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let hangup = loop {
        if let Some(ev) = capture
            .received
            .lock()
            .unwrap()
            .iter()
            .find(|v| {
                v["event_type"].as_str() == Some("call_hangup") && v["event"]["leg_id"].is_null()
            })
            .cloned()
        {
            break ev;
        }
        if tokio::time::Instant::now() >= hangup_deadline {
            panic!("the call must end cleanly after the caller presses 9");
        }
        sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(
        hangup["event"]["session_id"].as_str(),
        Some(original_session_id.as_str()),
        "the session-level hangup must close the original session: {hangup}"
    );

    let _ = caller_rtp_task.abort();
    let _ = pump.abort();

    // ── On-disk artifacts: exactly ONE wav — the agent segment ────────────
    let record_dir = harness._record_dir.path();
    // The local archival layout nests under a daily subdir ({path}/YYYYMMDD).
    fn collect_wavs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_wavs(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "wav") {
                out.push(path);
            }
        }
    }
    let mut wavs: Vec<std::path::PathBuf> = Vec::new();
    collect_wavs(record_dir, &mut wavs);
    wavs.sort();
    assert_eq!(
        wavs.len(),
        1,
        "agent-only recording must produce exactly one wav (no full-call/IVR artifacts): {wavs:?}"
    );
    let wav_path = wavs[0].clone();
    assert!(
        wav_path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(&format!("_{AGENT}.wav"))),
        "the wav must be the agent segment: {}",
        wav_path.display()
    );

    // ── Content: stereo, both parties' tones present ──────────────────────
    let mut reader = rustpbx::media::wav_reader::WavReader::open(&wav_path)?;
    let spec = *reader.spec();
    assert_eq!(
        spec.channels, 2,
        "the agent segment must be stereo (caller + agent channels)"
    );
    let wav_frames = reader.duration() as f64 / spec.sample_rate as f64;
    // The on-disk duration must stay inside the agent stage — a recorder that
    // kept running after the REFER would exceed it. It must also match the
    // reported segment duration (±1.5 s flush granularity).
    let agent_stage_secs = (bye_at.saturating_sub(connect_ms)) as f64 / 1000.0;
    assert!(
        wav_frames <= agent_stage_secs + 3.0,
        "recorded wav spans {wav_frames:.1}s but the agent stage was {agent_stage_secs:.1}s — \
         the recorder kept rolling after the agent REFER/hangup"
    );
    assert!(
        (wav_frames - recorded_secs as f64).abs() <= 1.5,
        "wav duration {wav_frames:.1}s diverges from record_stopped duration {recorded_secs}s"
    );
    let samples: Vec<i16> = reader
        .samples()
        .map(|s| s.expect("wav sample must decode"))
        .collect();
    let left: Vec<i16> = samples.iter().step_by(2).copied().collect();
    let right: Vec<i16> = samples.iter().skip(1).step_by(2).copied().collect();
    let left_rms = rms(&left);
    let right_rms = rms(&right);
    assert!(
        left_rms > 500.0,
        "caller channel (left) is silent (rms={left_rms:.0}) — customer audio missing from the agent recording"
    );
    assert!(
        right_rms > 500.0,
        "agent channel (right) is silent (rms={right_rms:.0}) — agent audio missing from the agent recording"
    );

    // ── Agent release: the REFER-exit must free the agent (wrapup/idle) ───
    let agent_release_deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let busy = harness
            .cc_registry
            .get_agent(AGENT)
            .await
            .map(|a| matches!(a.status, rustpbx::addons::cc::agent::AgentStatus::Busy { .. }))
            .unwrap_or(true);
        if !busy || Instant::now() > agent_release_deadline {
            assert!(
                !busy,
                "agent must leave Busy after the REFER departure (wrapup/idle)"
            );
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    // ── CDR contract: the segment lands in the call record ────────────────
    let records = harness
        .server
        .cdr_capture
        .wait_for_records(1, Duration::from_secs(8))
        .await
        .ok_or_else(|| anyhow!("CDR must be generated"))?;
    let record = &records[0];
    assert_eq!(
        record.recorder.len(),
        1,
        "CDR recorder[] must carry exactly the agent segment: {:?}",
        record.recorder
    );
    assert_eq!(
        record.recorder[0].path, wav_path.to_string_lossy(),
        "CDR recorder[0] must point at the agent segment wav"
    );
    let segments = record
        .details
        .metadata
        .as_ref()
        .and_then(|m| m.get("recording_segments"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(segments.len(), 1, "exactly one recording segment: {segments:?}");
    assert_eq!(
        segments[0]["segmentType"].as_str(),
        Some("agent"),
        "segment_type must be agent: {segments:?}"
    );
    assert_eq!(
        segments[0]["label"].as_str(),
        Some(AGENT),
        "segment label must be the agent id: {segments:?}"
    );

    harness.server.stop();
    Ok(())
}
