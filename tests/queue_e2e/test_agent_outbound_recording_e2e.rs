//! e2e: agent OUTBOUND call (CTI click-to-dial, the `POST /cc/calls/originate`
//! path) under the "agent-only" recording configuration:
//!
//! - `[recording] auto_start = false` — nothing records by policy
//! - cc.toml `[recording] record_on_agent_connect = true`
//!   (`CcConfig::default()`) — the CC session hook starts a
//!   `segment_type = "agent"` segment when the agent-attributed originate is
//!   ANSWERED (agent Idle → Busy), even though the agent has no SIP leg here
//!   (the customer leg IS the session's A/caller leg).
//!
//! Pins:
//! 1. the agent segment starts at connect and is finalized at hangup —
//!    `record_stopped` with the `_<agent>.wav` name and a duration that
//!    matches the customer talk window (not the ringing/setup time);
//! 2. the WAV actually contains the customer's audio (content-level check);
//! 3. the CDR carries the recording (`recorder[]` + `recording_segments`)
//!    so consumers (cc call records, upload/notify pipeline) see it;
//! 4. the agent is attributed and released (Idle → Busy → wrapup/idle).

use anyhow::{Result, anyhow};
use rustpbx::addons::cc::acd::{AcdConfig, AcdEngine};
use rustpbx::addons::cc::agent::AgentRegistry as CcAgentRegistry;
use rustpbx::addons::cc::agent_registry_adapter::CcAgentRegistryAdapter;
use rustpbx::addons::cc::cc_call_session_hook::CcCallSessionHook;
use rustpbx::addons::cc::config::CcConfig;
use rustpbx::addons::cc::metrics::MetricsCollector;
use rustpbx::call::user::SipUser;
use rustpbx::config::{LocatorWebhookConfig, ProxyConfig, RecordingPolicy};
use rustpbx::proxy::active_call_registry::ActiveProxyCallRegistry;
use rustpbx::rwi::{RwiGateway, RwiGatewayRef, processor::RwiCommandProcessor, session::OriginateRequest, webhook::start_rwi_webhook_handler};
use sea_orm::Database;
use sea_orm_migration::MigratorTrait;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::time::sleep;

use crate::common::audio_analysis::rms;
use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::rtp_utils::{RtpPacket, extract_media_endpoint};
use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent, create_test_sdp_answer};
use crate::common::webhook_capture::WebhookCapture;

const AGENT: &str = "bob";
const CUSTOMER: &str = "customer";
/// How long the customer talks (and records) after answering the agent's
/// click-to-dial call.
const TALK_SECS: u64 = 2;

/// Loud μ-law tone byte for the customer's RTP (|amp| > 3000 when decoded).
const CUSTOMER_TONE_BYTE: u8 = 0x33;

const WEBHOOK_EVENTS: &[&str] = &[
    "call_created",
    "call_ringing",
    "call_answered",
    "call_hangup",
    "record_stopped",
];

fn proxy_config(record_path: &str) -> ProxyConfig {
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
    // auto start — only the cc agent segment may record.
    config.recording = Some(RecordingPolicy {
        enabled: Some(true),
        auto_start: Some(false),
        path: Some(record_path.to_string()),
        ..Default::default()
    });
    // The CTI originate dials the customer UA directly (no route table).
    config.route_originated_calls = false;
    config
}

struct OutboundHarness {
    server: E2eTestServer,
    gateway: RwiGatewayRef,
    cc_registry: Arc<CcAgentRegistry>,
    /// Keeps the record dir alive for the whole test.
    _record_dir: tempfile::TempDir,
}

async fn start_harness(capture: &WebhookCapture) -> Result<OutboundHarness> {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    rustpbx::addons::cc::migration::Migrator::up(&db, None).await.unwrap();
    let cc_registry = Arc::new(CcAgentRegistry::with_db(db));
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
    let mut proxy_config = proxy_config(&record_dir.path().to_string_lossy());
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
            username: CUSTOMER.to_string(),
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
            rwi_gateway: Some(gateway.clone()),
            #[cfg(feature = "addon-cc")]
            cc_policy_db: None,
        },
    )
    .await?;
    *call_registry_container.write().await = Some(server.registry.clone());

    Ok(OutboundHarness {
        server,
        gateway,
        cc_registry,
        _record_dir: record_dir,
    })
}

fn make_ua(proxy_addr: std::net::SocketAddr, username: &str) -> TestUa {
    TestUa::new(TestUaConfig {
        webrtc: false,
        username: username.to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(29700),
        proxy_addr,
    })
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

fn count_events(capture: &WebhookCapture, event_type: &str) -> usize {
    capture
        .received
        .lock()
        .unwrap()
        .iter()
        .filter(|v| v["event_type"].as_str() == Some(event_type))
        .count()
}

/// Agent OUTBOUND (CTI click-to-dial): the originate dials the customer with
/// the agent's identity; the agent segment must start when the call is
/// answered and capture the customer's audio.
#[test]
fn test_agent_outbound_originate_records_agent_segment() {
    run_with_big_stack(test_agent_outbound_originate_records_agent_segment_impl());
}

async fn test_agent_outbound_originate_records_agent_segment_impl() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    let capture = WebhookCapture::start().await;
    let harness = start_harness(&capture).await?;
    let epoch = Instant::now();

    // ── Customer UA + media: answers the click-to-dial INVITE ─────────────
    let customer_rtp = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
    let customer_endpoint_slot: Arc<std::sync::Mutex<Option<std::net::SocketAddr>>> =
        Arc::new(std::sync::Mutex::new(None));
    let customer_dialog_slot: Arc<std::sync::Mutex<Option<rsipstack::dialog::DialogId>>> =
        Arc::new(std::sync::Mutex::new(None));
    let established = Arc::new(AtomicUsize::new(0));
    let answered_at_ms: Arc<std::sync::atomic::AtomicU64> =
        Arc::new(std::sync::atomic::AtomicU64::new(0));

    let mut customer = make_ua(harness.server.proxy_addr, CUSTOMER);
    customer.start().await?;
    customer.register().await?;
    let customer_sip_port = customer.local_port();
    {
        let ua = customer.clone();
        let rtp_port = customer_rtp.local_addr()?.port();
        let endpoint_slot = customer_endpoint_slot.clone();
        let established = established.clone();
        let answered_at_ms = answered_at_ms.clone();
        let customer_dialog_slot: Arc<std::sync::Mutex<Option<rsipstack::dialog::DialogId>>> =
            customer_dialog_slot.clone();
        tokio::spawn(async move {
            loop {
                match ua.process_dialog_events().await {
                    Ok(events) => {
                        for ev in events {
                            match ev {
                                TestUaEvent::IncomingCall(dialog_id, offer) => {
                                    let _ = ua.ring_call(&dialog_id).await;
                                    sleep(Duration::from_millis(300)).await;
                                    if let Some(offer) = offer.as_deref()
                                        && let Some(endpoint) = extract_media_endpoint(offer)
                                    {
                                        *endpoint_slot.lock().unwrap() = Some(endpoint);
                                    }
                                    let sdp = offer
                                        .as_deref()
                                        .map(|o| create_test_sdp_answer(o, "127.0.0.1", rtp_port))
                                        .unwrap_or_else(|| {
                                            crate::common::test_ua::create_test_sdp("127.0.0.1", rtp_port, false)
                                        });
                                    let _ = ua.answer_call(&dialog_id, Some(sdp)).await;
                                    *customer_dialog_slot.lock().unwrap() = Some(dialog_id);
                                }
                                TestUaEvent::CallEstablished(_) => {
                                    answered_at_ms.store(
                                        epoch.elapsed().as_millis() as u64,
                                        std::sync::atomic::Ordering::Relaxed,
                                    );
                                    established.fetch_add(1, Ordering::Relaxed);
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(_) => break,
                }
                sleep(Duration::from_millis(30)).await;
            }
        });
    }

    // ── CTI originate: the agent dials the customer ───────────────────────
    let call_id = format!("cti-originate-{}", uuid::Uuid::new_v4());
    let customer_addr = format!("sip:{CUSTOMER}@127.0.0.1:{customer_sip_port}");
    let processor = RwiCommandProcessor::new(
        harness.server.registry.clone(),
        harness.gateway.clone(),
        harness.server.server_ref.conference_manager.clone(),
    )
    .with_sip_server(harness.server.server_ref.clone());

    let result = processor
        .process_command(rustpbx::rwi::session::RwiCommandPayload::Originate(
            OriginateRequest {
                call_id: call_id.clone(),
                destination: customer_addr,
                // The agent's identity rides as caller — the CC hook resolves
                // the attribution from the originate metadata.
                caller_id: Some(format!("sip:{AGENT}@127.0.0.1")),
                timeout_secs: Some(10),
                extra_headers: Default::default(),
                agent: Some(AGENT.to_string()),
                trunk: None,
                route_originated_calls: Some(false),
                record: None, // NO inline record option — the agent segment
                              // must come from record_on_agent_connect.
            },
        ))
        .await;
    assert!(
        result.is_ok(),
        "CTI originate must dispatch: {:?}",
        result.err()
    );

    // ── Customer answers; the agent segment must arm on connect ───────────
    wait_webhook_event(&capture, "call_answered", Duration::from_secs(15))
        .await
        .expect("originate must be answered (call_answered)");
    let established_deadline = Instant::now() + Duration::from_secs(5);
    while established.load(Ordering::Relaxed) == 0 && Instant::now() < established_deadline {
        sleep(Duration::from_millis(30)).await;
    }
    assert_eq!(
        established.load(Ordering::Relaxed),
        1,
        "customer UA must be established with the originate"
    );
    let connect_at_ms = epoch.elapsed().as_millis() as u64;

    // Agent went Idle → Busy via the click-to-dial attribution.
    let agent_busy = harness
        .cc_registry
        .get_agent(AGENT)
        .await
        .map(|a| matches!(a.status, rustpbx::addons::cc::agent::AgentStatus::Busy { .. }))
        .unwrap_or(false);
    assert!(
        agent_busy,
        "the agent must be Busy while their outbound call is up"
    );

    // ── Customer talk window: tone toward the PBX (recorded on leg A) ─────
    let pbx_customer_endpoint = customer_endpoint_slot
        .lock()
        .unwrap()
        .ok_or_else(|| anyhow!("customer leg media endpoint unknown"))?;
    {
        let mut seq: u16 = 700;
        let mut ts: u32 = 7000;
        for _ in 0..(TALK_SECS * 50) {
            let packet = RtpPacket::new(0, seq, ts, 0x3333, vec![CUSTOMER_TONE_BYTE; 160]);
            customer_rtp
                .send_to(&packet.encode(), pbx_customer_endpoint)
                .await?;
            seq = seq.wrapping_add(1);
            ts = ts.wrapping_add(160);
            sleep(Duration::from_millis(20)).await;
        }
    }

    // ── Customer hangs up; the session finalize must close the segment ────
    let customer_dialog = customer_dialog_slot
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| anyhow!("customer dialog unknown"))?;
    customer.hangup(&customer_dialog).await.ok();

    let stopped = wait_webhook_event(&capture, "record_stopped", Duration::from_secs(10))
        .await
        .ok_or_else(|| anyhow!("record_stopped for the agent outbound segment is missing"))?;
    assert_eq!(
        count_events(&capture, "record_stopped"),
        1,
        "exactly ONE recording segment (the agent's) must exist"
    );
    assert_eq!(
        stopped["call_id"].as_str(),
        Some(call_id.as_str()),
        "record_stopped must belong to the originate call: {stopped}"
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

    // The segment covers the customer talk window…
    assert!(
        recorded_secs >= TALK_SECS as i64 - 1,
        "agent recording ran only {recorded_secs}s — shorter than the {TALK_SECS}s talk window"
    );
    // …and starts at ANSWER (agent connect), not at the originate INVITE:
    // ringing + answer setup took ~0.3-1 s before the talk window opened.
    let talk_end_ms = epoch.elapsed().as_millis() as u64;
    let max_stage_secs = (talk_end_ms.saturating_sub(connect_at_ms)) as f64 / 1000.0 + 3.0;
    assert!(
        recorded_secs as f64 <= max_stage_secs,
        "agent recording ran {recorded_secs}s — grew past the answered stage ({max_stage_secs:.1}s max)"
    );

    // Agent released after hangup (wrapup or idle — never stuck Busy).
    let release_deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let busy = harness
            .cc_registry
            .get_agent(AGENT)
            .await
            .map(|a| matches!(a.status, rustpbx::addons::cc::agent::AgentStatus::Busy { .. }))
            .unwrap_or(true);
        if !busy || Instant::now() > release_deadline {
            assert!(!busy, "agent must leave Busy after the outbound hangup");
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    // ── On-disk artifact + content: the customer's tone must be present ───
    let record_dir = harness._record_dir.path();
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
    assert_eq!(
        wavs.len(),
        1,
        "agent-only recording must produce exactly one wav: {wavs:?}"
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

    let mut reader = rustpbx::media::wav_reader::WavReader::open(&wav_path)?;
    let spec = *reader.spec();
    let wav_frames = reader.duration() as f64 / spec.sample_rate as f64;
    assert!(
        (wav_frames - recorded_secs as f64).abs() <= 1.5,
        "wav duration {wav_frames:.1}s diverges from record_stopped duration {recorded_secs}s"
    );
    // The originate is a one-legged bridge (no B endpoint), so the recorder
    // carries the customer leg: at least one channel must hold the tone.
    let samples: Vec<i16> = reader
        .samples()
        .map(|s| s.expect("wav sample must decode"))
        .collect();
    let channels = spec.channels.max(1) as usize;
    let mut any_channel_loud = false;
    for ch in 0..channels {
        let channel: Vec<i16> = samples.iter().skip(ch).step_by(channels).copied().collect();
        if rms(&channel) > 500.0 {
            any_channel_loud = true;
        }
    }
    assert!(
        any_channel_loud,
        "the agent outbound recording is silent — customer audio missing"
    );

    // ── CDR contract: the originate CDR carries the agent segment ─────────
    let records = harness
        .server
        .cdr_capture
        .wait_for_records(1, Duration::from_secs(8))
        .await
        .ok_or_else(|| anyhow!("CDR must be generated for the originate"))?;
    let record = records
        .iter()
        .find(|r| r.call_id == call_id)
        .ok_or_else(|| anyhow!("CDR for the originate call {call_id} is missing"))?;
    assert_eq!(
        record.recorder.len(),
        1,
        "CDR recorder[] must carry exactly the agent segment: {:?}",
        record.recorder
    );
    assert_eq!(
        record.recorder[0].path,
        wav_path.to_string_lossy(),
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
    assert_eq!(
        segments.len(),
        1,
        "CDR metadata.recording_segments must list the agent segment: {segments:?}"
    );
    assert_eq!(
        segments[0]["segmentType"].as_str(),
        Some("agent"),
        "segment_type must be agent: {segments:?}"
    );

    harness.server.stop();
    Ok(())
}

/// Big-stack driver: the full SIP session + originate future chain needs more
/// than the default 2 MiB tokio thread stack in debug builds (production uses
/// 8 MiB, see src/bin/rustpbx.rs).
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
            .expect("e2e agent outbound recording test failed");
    });
}
