//! E2E: AMD (answering machine / AI assistant) detection over the REAL
//! outbound call + media + RWI webhook path.
//!
//! Contract: with the resolved `amd` config enabled (here: global
//! `[proxy] amd.enabled = true`), the outbound callee leg arms the detector
//! the moment bob answers, and the audio bob streams back — a synthetic
//! "greeting" burst followed by the classic sustained ~1 kHz voicemail beep —
//! must surface on the RWI webhook as a `call_amd_detected` event with
//! `result = "answering_machine"` and `beep_detected = true`.
//!
//! There is no other path the verdict can take: the beep rides the ordinary
//! audio RTP stream (PCMU), and the classification happens on the callee
//! leg's shared analysis tap (PcmAnalysisTap). The webhook allow-list is
//! extended by `call_amd_detected` so the event is observable.

use anyhow::{Result, anyhow};
use rustpbx::call::user::SipUser;
use rustpbx::config::{LocatorWebhookConfig, MediaProxyMode, ProxyConfig};
use rustpbx::media::amd::AmdConfig;
use rustpbx::rwi::{RwiGateway, RwiGatewayRef, webhook::start_rwi_webhook_handler};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::rtp_utils::{RtpEndpoint, extract_media_endpoint};
use crate::common::test_helpers::make_sdp;
use crate::common::test_ua::TestUaEvent;
use crate::common::webhook_capture::WebhookCapture;

const CALLER: &str = "amd_caller";
const CALLEE: &str = "amd_callee";
const SAMPLE_RATE: u32 = 8000;
const BLOCK_SAMPLES: usize = 160; // 20 ms @ 8 kHz

/// Webhook allow-list for THIS scenario: the lifecycle set plus
/// `call_amd_detected` — the event this test asserts on.
const AMD_WEBHOOK_EVENTS: &[&str] = &[
    "call_created",
    "call_answered",
    "call_hangup",
    "call_amd_detected",
];

// ── synthetic remote-party audio ───────────────────────────────────────────

/// Pseudo-noise PCM (RMS ≈ 4600): a "speech" surrogate the heuristic's RMS
/// tracker counts as a greeting run. A seeded LCG keeps the fixture stable.
fn noise_pcm(duration_ms: u64, seed: u64) -> Vec<i16> {
    let n = SAMPLE_RATE as u64 * duration_ms / 1000;
    let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (0..n)
        .map(|_| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((x >> 33) as i64 % 8000) as i16
        })
        .collect()
}

fn silence_pcm(duration_ms: u64) -> Vec<i16> {
    vec![0i16; SAMPLE_RATE as usize * duration_ms as usize / 1000]
}

fn tone_pcm(duration_ms: u64, freq_hz: f64) -> Vec<i16> {
    let n = SAMPLE_RATE as u64 * duration_ms / 1000;
    (0..n)
        .map(|i| {
            let t = i as f64 / SAMPLE_RATE as f64;
            ((2.0 * std::f64::consts::PI * freq_hz * t).sin() * 8000.0) as i16
        })
        .collect()
}

/// PCMU payloads (20 ms frames) for the whole voicemail imitation:
/// 1 s "greeting" noise → 150 ms gap → 400 ms 1 kHz beep. The gap is shorter
/// than the heuristic's 800 ms pause threshold, so the greeting run stays
/// open and the beep decides the verdict ≈1.6 s after answer.
fn voicemail_pcmu_payloads() -> Vec<Vec<u8>> {
    let mut pcm = noise_pcm(1000, 42);
    pcm.extend_from_slice(&silence_pcm(150));
    pcm.extend_from_slice(&tone_pcm(400, 1000.0));
    let mut encoder = audio_codec::create_encoder(audio_codec::CodecType::PCMU);
    pcm.chunks(BLOCK_SAMPLES)
        .map(|frame| encoder.encode(frame))
        .collect()
}

// ── harness ────────────────────────────────────────────────────────────────

async fn start_harness(capture: &WebhookCapture) -> Result<E2eTestServer> {
    let gateway: RwiGatewayRef = Arc::new(parking_lot::RwLock::new({
        let mut gw = RwiGateway::new();
        gw.set_webhook_tx(start_rwi_webhook_handler(
            LocatorWebhookConfig {
                url: capture.url.clone(),
                events: AMD_WEBHOOK_EVENTS.iter().map(|s| s.to_string()).collect(),
                headers: None,
                timeout_ms: Some(5000),
                retries: None,
                track_queue_latency: None,
            },
            rustpbx::rwi::webhook::WEBHOOK_CHANNEL_SIZE,
        ));
        gw
    }));

    let mut proxy_config = ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(0),
        modules: Some(vec![
            "auth".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]),
        ..Default::default()
    };
    proxy_config.ensure_user = Some(false);
    proxy_config.enable_latching = false;
    proxy_config.media_proxy = MediaProxyMode::All;
    // Global AMD default: enabled, with a bounded window so a misfire cannot
    // stall the test. Outbound calls route via the registered-user locator
    // (no explicit routes needed — mirrors test_outbound_e2e).
    proxy_config.amd = Some(AmdConfig {
        enabled: Some(true),
        timeout_secs: Some(8),
        ..Default::default()
    });

    let users = vec![
        SipUser {
            id: 1,
            username: CALLER.to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        },
        SipUser {
            id: 2,
            username: CALLEE.to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        },
    ];

    E2eTestServer::start_with_inject(
        proxy_config,
        E2eTestServerInject {
            users,
            rwi_gateway: Some(gateway),
            ..Default::default()
        },
    )
    .await
}

async fn wait_webhook_event(
    capture: &WebhookCapture,
    event_type: &str,
    timeout: Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        {
            let events = capture.received.lock().unwrap();
            if events
                .iter()
                .any(|v| v["event_type"].as_str() == Some(event_type))
            {
                return Ok(());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let types: Vec<String> = {
                let events = capture.received.lock().unwrap();
                events
                    .iter()
                    .map(|v| v["event_type"].as_str().unwrap_or("?").to_string())
                    .collect()
            };
            return Err(anyhow!(
                "timed out waiting for webhook event '{event_type}'; got {types:?}"
            ));
        }
        sleep(Duration::from_millis(50)).await;
    }
}

// ── the scenario ───────────────────────────────────────────────────────────

#[tokio::test]
async fn test_amd_detects_voicemail_on_outbound_callee_leg() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    let capture = WebhookCapture::start().await;
    let server = Arc::new(start_harness(&capture).await?);

    let alice = Arc::new(server.create_ua(CALLER).await?);
    // create_ua registers the callee so the outbound INVITE can locate him.
    let mut bob = server.create_ua(CALLEE).await?;
    sleep(Duration::from_millis(300)).await;

    // Bob's answer advertises a plain UDP port the test will send RTP from
    // (the UA itself never transmits — same shape as the inband DTMF e2e).
    let bob_rtp_port = portpicker::pick_unused_port().unwrap_or(33100);
    let callee_sdp = make_sdp(bob_rtp_port);

    // Alice offers PCMU from a port nothing listens on (her audio is not
    // needed for the AMD path; only the callee leg is analysed).
    let caller_rtp_port = portpicker::pick_unused_port().unwrap_or(33200);
    let caller_sdp = make_sdp(caller_rtp_port);

    let caller = tokio::spawn({
        let a = alice.clone();
        let sdp = caller_sdp.clone();
        async move { a.make_call(CALLEE, Some(sdp)).await }
    });

    // Wait for the server's INVITE at bob and answer; capture the offer SDP —
    // it names the callee leg's media endpoint (where bob's audio must land).
    let mut server_offer = None;
    let mut bob_dialog_id = None;
    for _ in 0..50 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, offer) = event {
                bob_dialog_id = Some(id.clone());
                bob.answer_call(&id, Some(callee_sdp.clone())).await?;
                server_offer = offer;
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    let bob_dialog_id = bob_dialog_id.expect("Bob should receive the outbound call");
    let server_offer =
        server_offer.expect("IncomingCall must carry the server's callee-leg offer SDP");

    let alice_id = match tokio::time::timeout(Duration::from_secs(5), caller).await {
        Ok(Ok(Ok(id))) => Some(id),
        _ => None,
    };
    assert!(alice_id.is_some(), "Call should establish");

    // Stream the voicemail imitation from bob's advertised port into the
    // callee leg's media endpoint. Detection arms at the 200 OK; the verdict
    // lands ≈1.6 s later, well inside the 8 s window.
    let server_media = extract_media_endpoint(&server_offer)
        .expect("server offer must carry an audio media endpoint");
    let endpoint = RtpEndpoint::bind(bob_rtp_port).await?;
    endpoint
        .send_pcmu_blocking(server_media, voicemail_pcmu_payloads(), 20)
        .await?;

    // The AMD verdict must reach the webhook with the machine classification.
    wait_webhook_event(&capture, "call_amd_detected", Duration::from_secs(15)).await?;
    let verdicts: Vec<serde_json::Value> = {
        let events = capture.received.lock().unwrap();
        events
            .iter()
            .filter(|b| b["event_type"].as_str() == Some("call_amd_detected"))
            .map(|b| b["event"].clone())
            .collect()
    };
    let verdict = verdicts
        .first()
        .expect("call_amd_detected event must carry a payload");
    assert_eq!(
        verdict["result"].as_str(),
        Some("answering_machine"),
        "greeting + beep must classify as answering_machine: {verdict}"
    );
    assert_eq!(
        verdict["beep_detected"].as_bool(),
        Some(true),
        "the 1 kHz beep must be reported: {verdict}"
    );
    assert_eq!(
        verdict["engine"].as_str(),
        Some("heuristic"),
        "v1 ships the heuristic engine: {verdict}"
    );
    assert!(
        verdict["confidence"].as_f64().is_some_and(|c| (0.0..=1.0).contains(&c)),
        "confidence must be normalized: {verdict}"
    );

    endpoint.stop();
    bob.hangup(&bob_dialog_id).await.ok();
    sleep(Duration::from_millis(300)).await;
    server.stop();
    Ok(())
}
